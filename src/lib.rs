use futures::{Future, IntoFuture, Stream};
use serde_derive::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ObjectWrapper {
    object: serde_json::Value,
}

#[derive(Deserialize, Debug)]
pub struct EventItem {
    pub created: u64,
    pub data: ObjectWrapper,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug)]
struct QueryError(String);

impl From<tokio_postgres::Error> for QueryError {
    fn from(err: tokio_postgres::Error) -> QueryError {
        QueryError(format!("{:?}", err))
    }
}

fn tack_on<T, E, A>(src: Result<T, E>, add: A) -> Result<(T, A), (E, A)> {
    match src {
        Ok(value) => Ok((value, add)),
        Err(err) => Err((err, add)),
    }
}

fn to_timestamp(stamp: u64) -> std::time::SystemTime {
    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(stamp, 0)
}

type OHHttpClient = std::sync::Arc<hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>>;

pub struct Otterhound {
    auth_header: String,
    db_pool: bb8::Pool<bb8_postgres::PostgresConnectionManager<tokio_postgres::NoTls>>,
    http_client: OHHttpClient,
}

impl Otterhound {
    pub fn new_with_some(auth_header: String, http_client: OHHttpClient) -> impl Future<Item=Self, Error=String> + Send {
        bb8::Pool::builder()
            .build(bb8_postgres::PostgresConnectionManager::new(
                    std::env::var("DATABASE_URL").expect("Missing DATABASE_URL"),
                    tokio_postgres::NoTls
                ))
            .map_err(|err| format!("Failed to initialize database pool: {:?}", err))
            .map(|db_pool| {
                Otterhound {
                    auth_header,
                    db_pool,
                    http_client,
                }
            })
    }

    pub fn handle_event(&self, evt: EventItem) -> Box<Future<Item=(), Error=String> + Send> {
        println!("Received event: {}", evt.type_);

        match evt.type_.as_ref() {
            "checkout.session.completed" => {
                println!("{:?}", evt.data);

                #[derive(Deserialize)]
                struct CheckoutSession {
                    id: String,
                    subscription: String,
                }

                Box::new(serde_json::from_value(evt.data.object)
                         .map_err(|err| format!("Failed to parse object: {:?}", err))
                         .and_then(|session: CheckoutSession| {
                             let db_pool = self.db_pool.clone();

                             #[derive(Deserialize)]
                             struct Subscription {
                                 created: u64,
                                 current_period_end: u64,
                             }

                             let session_id = session.id;
                             let sub_id = session.subscription;
                             let auth_header: &str = &self.auth_header;

                             hyper::Request::get(&format!("https://api.stripe.com/v1/subscriptions/{}", sub_id))
                                 .header("Authorization", auth_header)
                                 .body(hyper::Body::empty())
                                 .map_err(|err| format!("Failed to construct request: {:?}", err))
                                 .map(move |req| {
                                     self.http_client.request(req)
                                         .and_then(|res| {
                                             let status = res.status();
                                             res.into_body().concat2()
                                                 .map(move |body| (body, status))
                                         })
                                     .map_err(|err| format!("Failed to send request: {:?}", err))
                                         .and_then(|(body, status)| {
                                             if status.is_success() {
                                                 serde_json::from_slice(&body)
                                                     .map_err(|err| format!("Failed to parse response: {:?}", err))
                                             } else {
                                                 Err(format!("Received error from API: {:?}", body))
                                             }
                                         })
                                     .and_then(move |sub: Subscription| {
                                         db_pool.run(|mut conn| {
                                             conn.prepare("UPDATE subscription_checkout_sessions SET completed=TRUE WHERE stripe_id=$1 AND completed=FALSE RETURNING user_id, tier_id")
                                                 .join(conn.prepare("INSERT INTO user_subscriptions (tier, user_id, start_timestamp, end_timestamp, stripe_subscription) VALUES ($1, $2, $3, $4, $5)"))
                                                 .map_err(|err| format!("Failed to prepare queries: {:?}", err))
                                                 .then(|res| tack_on(res, conn))
                                                 .and_then(|((st1, st2), mut conn)| {
                                                     conn.simple_query("BEGIN")
                                                         .into_future()
                                                         .map_err(|(err, _)| format!("Failed to start transaction: {:?}", err))
                                                         .then(|res| tack_on(res, conn))
                                                         .and_then(move |(_, mut conn)| {
                                                             conn.query(&st1, &[&session_id])
                                                                 .into_future()
                                                                 .map(|(res, _)| res)
                                                                 .map_err(|(err, _)| format!("Failed to query for session: {:?}", err))
                                                                 .then(|res| tack_on(res, conn))
                                                                 .and_then(|(row, conn)| {
                                                                     match row {
                                                                         Some(row) => {
                                                                             Ok(((row.get(0), row.get(1)), conn))
                                                                         },
                                                                         None => Err(("Couldn't find the session".to_owned(), conn)),
                                                                     }
                                                                 })
                                                             .and_then(move |((user_id, tier_id), mut conn): ((i32, i32), _)| {
                                                                 conn.execute(&st2, &[&tier_id, &user_id, &to_timestamp(sub.created), &to_timestamp(sub.current_period_end), &sub_id])
                                                                     .map_err(|err| format!("Failed to add subscription: {:?}", err))
                                                                     .then(|res| tack_on(res, conn))
                                                             })
                                                         })
                                                     .and_then(|(_, mut conn)| {
                                                         conn.simple_query("COMMIT").into_future()
                                                             .map(|_| ())
                                                             .map_err(|(err, _)| format!("Failed to commit transaction: {:?}", err))
                                                             .then(|res| tack_on(res, conn))
                                                     })
                                                         .or_else(|(err, mut conn)| conn.simple_query("ROLLBACK").into_future().then(|_| Err((err, conn))))
                                                 })
                                             .map_err(|(err, conn)| (QueryError(err), conn))
                                         })
                                         .map_err(|err| format!("{:?}", err))
                                     })
                                 })
                         })
                             .into_future()
                                 .and_then(|x| x)
                )
            },
            _ => Box::new(futures::future::ok(())),
        }
    }
}
