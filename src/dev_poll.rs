use futures::{Future, Stream};
use serde_derive::Deserialize;

use otterhound::EventItem;

#[derive(Deserialize, Debug)]
struct EventListResponse {
    data: Vec<EventItem>,
}

fn main() {
    let auth_header = otterhound::gen_auth_header();
    let auth_header: &str = &auth_header;

    let mut runtime = tokio::runtime::Runtime::new().expect("Failed to initialize Tokio");

    let client = {
        let connector =
            hyper_tls::HttpsConnector::new(2).expect("Failed to initialize HTTPS client");
        std::sync::Arc::new(hyper::Client::builder().build(connector))
    };

    let otterhound = {
        let auth_header = auth_header.to_owned();
        let client = client.clone();
        runtime
            .block_on(futures::future::lazy(|| {
                otterhound::Otterhound::new_with_some(auth_header, client)
            }))
            .expect("Failed to initialize")
    };

    let mut last_ts: Option<u64> = None;

    loop {
        let result = hyper::Request::get(&format!(
            "https://api.stripe.com/v1/events{}",
            match last_ts {
                Some(last_ts) => format!("?created[gt]={}", last_ts),
                None => "".to_owned(),
            }
        ))
        .header("Authorization", auth_header)
        .body(hyper::Body::empty())
        .map_err(|err| format!("Failed to construct request: {:?}", err))
        .and_then(|req| {
            runtime
                .block_on(client.request(req).and_then(|res| {
                    let status = res.status();
                    res.into_body().concat2().map(move |body| (body, status))
                }))
                .map_err(|err| format!("Failed to send request: {:?}", err))
        })
        .and_then(|(body, status)| {
            if status.is_success() {
                serde_json::from_slice(&body)
                    .map_err(|err| format!("Failed to parse response: {:?}", err))
            } else {
                Err(format!("Received error from API: {:?}", body))
            }
        })
        .and_then(|resp: EventListResponse| {
            let new_last_ts = resp.data.iter().map(|item| item.created).max();
            if let Some(new_last_ts) = new_last_ts {
                let old_last_ts = std::mem::replace(&mut last_ts, Some(new_last_ts));

                if let Some(_) = old_last_ts {
                    for item in resp.data {
                        runtime.spawn(
                            otterhound
                                .handle_event(item)
                                .map_err(|err| eprintln!("Error handling event: {}", err)),
                        );
                    }
                } else {
                    println!("Got first batch, enabling");
                }
            }

            Ok(())
        });

        if let Err(err) = result {
            eprintln!("Error in loop: {:?}", err);
        }

        std::thread::sleep(std::time::Duration::new(2, 0));
    }
}
