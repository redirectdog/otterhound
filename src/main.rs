use futures::{Future, IntoFuture, Stream};
use hmac::crypto_mac::Mac;
use std::sync::Arc;

const MAX_TIME_DIFF: std::time::Duration = std::time::Duration::from_secs(60 * 5);

struct ServerState {
    signing_secret: String,
    otterhound: otterhound::Otterhound,
}

fn handle_request(
    req: hyper::Request<hyper::Body>,
    state: Arc<ServerState>,
) -> impl Future<Item = hyper::Response<hyper::Body>, Error = hyper::Error> + Send {
    req.headers()
        .get("Stripe-Signature")
        .ok_or_else(|| "Missing Signature".to_owned())
        .and_then(|sig_data| {
            let mut timestamp = None;
            let mut signatures = Vec::new();
            sig_data
                .to_str()
                .map_err(|err| format!("Failed to read header: {:?}", err))?
                .split(',')
                .for_each(|pair| {
                    let mut spl = pair.split("=");
                    let key = spl.next().unwrap();
                    if key == "t" {
                        timestamp = spl.next().map(|x| x.to_owned());
                    } else if key == "v1" {
                        if let Some(sig) = spl.next() {
                            signatures.push(sig.to_owned());
                        }
                    }
                });

            timestamp
                .ok_or_else(|| "Missing timestamp".to_owned())
                .map(|timestamp| (timestamp, signatures))
        })
        .into_future()
        .and_then({
            let state = state.clone();
            |(timestamp, signatures)| {
                req.into_body()
                    .concat2()
                    .map_err(|err| format!("Failed reading body: {:?}", err))
                    .and_then(move |body| {
                        let signed_payload = {
                            let mut value = timestamp.as_bytes().to_vec();
                            value.push(b'.');
                            value.extend_from_slice(&body);
                            value
                        };

                        let mut mac =
                            hmac::Hmac::<sha2::Sha256>::new_varkey(state.signing_secret.as_bytes())
                                .unwrap();
                        mac.input(&signed_payload);
                        let expected = mac.result();

                        for sig in signatures {
                            let sig = hex::decode(sig);
                            if let Ok(sig) = sig {
                                if expected
                                    == hmac::crypto_mac::MacResult::new(
                                        generic_array::GenericArray::clone_from_slice(&sig),
                                    )
                                {
                                    return Ok((timestamp, body));
                                }
                            } else {
                                println!("Unable to parse signature");
                            }
                        }

                        Err("Signature validation failed".to_owned())
                    })
            }
        })
        .and_then(|(timestamp, body)| {
            let timestamp = timestamp
                .parse()
                .map_err(|err| format!("Failed to parse timestamp: {:?}", err))?;
            let timestamp =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp);

            let time_diff = match std::time::SystemTime::now().duration_since(timestamp) {
                Ok(time_diff) => time_diff,
                Err(err) => err.duration(),
            };

            if time_diff > MAX_TIME_DIFF {
                return Err("Timestamp is too far from current time".to_owned());
            }

            serde_json::from_slice(&body).map_err(|err| format!("Failed to parse body: {:?}", err))
        })
        .map(move |body| {
            tokio::spawn(
                state
                    .otterhound
                    .handle_event(body)
                    .map_err(|err| eprintln!("{}", err)),
            );

            hyper::Response::new(hyper::Body::empty())
        })
        .or_else(|err| {
            eprintln!("Error in request handler: {}", err);
            let mut res = hyper::Response::new("Internal Server Error".into());
            *res.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;

            Ok(res)
        })
}

fn main() {
    let port: u16 = match std::env::var("PORT").ok() {
        Some(port_str) => port_str.parse().expect("Failed to parse port"),
        None => 6868,
    };
    let signing_secret = std::env::var("SIGNING_SECRET").expect("Missing SIGNING_SECRET");

    tokio::run(
        otterhound::Otterhound::new()
            .and_then(move |otterhound| {
                let state = Arc::new(ServerState {
                    signing_secret,
                    otterhound,
                });

                hyper::Server::bind(&std::net::SocketAddr::from((
                    std::net::Ipv6Addr::UNSPECIFIED,
                    port,
                )))
                .serve(move || {
                    let state = state.clone();
                    hyper::service::service_fn(move |req| handle_request(req, state.clone()))
                })
                .map_err(|err| format!("Error running server: {:?}", err))
            })
            .map_err(|err| panic!("Failure: {:?}", err)),
    );
}
