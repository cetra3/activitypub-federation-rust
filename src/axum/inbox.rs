//! Handles incoming activities, verifying HTTP signatures and other checks
//!
#![doc = include_str!("../../docs/08_receiving_activities.md")]

use crate::{
    config::RequestData,
    error::Error,
    fetch::object_id::ObjectId,
    http_signatures::{verify_inbox_hash, verify_signature},
    traits::{ActivityHandler, Actor, ApubObject},
};
use axum::{
    async_trait,
    body::{Bytes, HttpBody},
    extract::FromRequest,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use http::{HeaderMap, Method, Uri};
use serde::de::DeserializeOwned;
use tracing::debug;

/// Handles incoming activities, verifying HTTP signatures and other checks
pub async fn receive_activity<Activity, ActorT, Datatype>(
    activity_data: ActivityData,
    data: &RequestData<Datatype>,
) -> Result<(), <Activity as ActivityHandler>::Error>
where
    Activity: ActivityHandler<DataType = Datatype> + DeserializeOwned + Send + 'static,
    ActorT: ApubObject<DataType = Datatype> + Actor + Send + 'static,
    for<'de2> <ActorT as ApubObject>::ApubType: serde::Deserialize<'de2>,
    <Activity as ActivityHandler>::Error: From<anyhow::Error>
        + From<Error>
        + From<<ActorT as ApubObject>::Error>
        + From<serde_json::Error>,
    <ActorT as ApubObject>::Error: From<Error> + From<anyhow::Error>,
    Datatype: Clone,
{
    verify_inbox_hash(activity_data.headers.get("Digest"), &activity_data.body)?;

    let activity: Activity = serde_json::from_slice(&activity_data.body)?;
    data.config.verify_url_and_domain(&activity).await?;
    let actor = ObjectId::<ActorT>::from(activity.actor().clone())
        .dereference(data)
        .await?;

    // TODO: why do errors here not get returned over http?
    verify_signature(
        &activity_data.headers,
        &activity_data.method,
        &activity_data.uri,
        actor.public_key(),
    )?;

    debug!("Receiving activity {}", activity.id().to_string());
    activity.receive(data).await?;
    Ok(())
}

/// Contains all data that is necessary to receive an activity from an HTTP request
#[derive(Debug)]
pub struct ActivityData {
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: Vec<u8>,
}

#[async_trait]
impl<S, B> FromRequest<S, B> for ActivityData
where
    Bytes: FromRequest<S, B>,
    B: HttpBody + Send + 'static,
    S: Send + Sync,
    <B as HttpBody>::Error: std::fmt::Display,
    <B as HttpBody>::Data: Send,
{
    type Rejection = Response;

    async fn from_request(req: Request<B>, _state: &S) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();

        // this wont work if the body is an long running stream
        let bytes = hyper::body::to_bytes(body)
            .await
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response())?;

        Ok(Self {
            headers: parts.headers,
            method: parts.method,
            uri: parts.uri,
            body: bytes.to_vec(),
        })
    }
}

// TODO: copy tests from actix-web inbox and implement for axum as well