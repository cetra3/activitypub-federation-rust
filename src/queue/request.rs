use std::time::{Duration, SystemTime};

use http::{header::HeaderName, HeaderMap, HeaderValue};
use httpdate::fmt_http_date;
use reqwest::Request;
use reqwest_middleware::ClientWithMiddleware;
use url::Url;

use crate::{
    error::Error,
    http_signatures::sign_request,
    queue::util::retry,
    reqwest_shim::ResponseExt,
    FEDERATION_CONTENT_TYPE,
};
use anyhow::{anyhow, Context};
use tracing::debug;

use super::{util::RetryStrategy, ActivityMessage};

/// Sign and send a message with an optional retry
/// The retry itself doesn't re-sign the request, so the retry times should be < 5 min
pub async fn sign_and_send(
    message: &ActivityMessage,
    client: &ClientWithMiddleware,
    timeout: Duration,
    retry_strategy: RetryStrategy,
    http_signature_compat: bool,
) -> Result<(), anyhow::Error> {
    debug!(
        "Sending {} to {}, contents:\n {}",
        message.activity_id,
        message.inbox,
        serde_json::from_slice::<serde_json::Value>(&message.activity)?
    );
    let request_builder = client
        .post(message.inbox.to_string())
        .timeout(timeout)
        .headers(generate_request_headers(&message.inbox));
    let request = sign_request(
        request_builder,
        &message.actor_id,
        message.activity.clone(),
        message.private_key.clone(),
        http_signature_compat,
    )
    .await
    .context("signing request")?;

    retry(
        || {
            send(
                message,
                client,
                request
                    .try_clone()
                    .expect("The body of the request is not cloneable"),
            )
        },
        retry_strategy,
    )
    .await
}

pub(super) async fn send(
    task: &ActivityMessage,
    client: &ClientWithMiddleware,
    request: Request,
) -> Result<(), anyhow::Error> {
    let response = client.execute(request).await;

    match response {
        Ok(o) if o.status().is_success() => {
            debug!(
                "Activity {} delivered successfully to {}",
                task.activity_id, task.inbox
            );
            Ok(())
        }
        Ok(o) if o.status().is_client_error() => {
            let text = o.text_limited().await.map_err(Error::other)?;
            debug!(
                "Activity {} was rejected by {}, aborting: {}",
                task.activity_id, task.inbox, text,
            );
            Ok(())
        }
        Ok(o) => {
            let status = o.status();
            let text = o.text_limited().await.map_err(Error::other)?;
            Err(anyhow!(
                "Queueing activity {} to {} for retry after failure with status {}: {}",
                task.activity_id,
                task.inbox,
                status,
                text,
            ))
        }
        Err(e) => Err(anyhow!(
            "Queueing activity {} to {} for retry after connection failure: {}",
            task.activity_id,
            task.inbox,
            e
        )),
    }
}

pub(crate) fn generate_request_headers(inbox_url: &Url) -> HeaderMap {
    let mut host = inbox_url.domain().expect("read inbox domain").to_string();
    if let Some(port) = inbox_url.port() {
        host = format!("{}:{}", host, port);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static(FEDERATION_CONTENT_TYPE),
    );
    headers.insert(
        HeaderName::from_static("host"),
        HeaderValue::from_str(&host).expect("Hostname is valid"),
    );
    headers.insert(
        "date",
        HeaderValue::from_str(&fmt_http_date(SystemTime::now())).expect("Date is valid"),
    );
    headers
}
