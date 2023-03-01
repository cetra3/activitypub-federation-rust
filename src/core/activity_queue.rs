use crate::{
    config::RequestData,
    core::http_signatures::sign_request,
    error::Error,
    traits::ActivityHandler,
    utils::reqwest_shim::ResponseExt,
    APUB_JSON_CONTENT_TYPE,
};
use anyhow::anyhow;
use background_jobs::{
    memory_storage::{ActixTimer, Storage},
    ActixJob,
    Backoff,
    Manager,
    MaxRetries,
    WorkerConfig,
};
use http::{header::HeaderName, HeaderMap, HeaderValue};
use httpdate::fmt_http_date;
use itertools::Itertools;
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};
use tracing::{debug, info, warn};
use url::Url;

/// Signs and delivers outgoing activities with retry.
///
/// The list of inboxes gets deduplicated (important for shared inbox). All inboxes on the local
/// domain and those which fail the [crate::config::UrlVerifier] check are excluded from delivery.
/// For each remaining inbox a background tasks is created. It signs the HTTP header with the given
/// private key. Finally the activity is delivered to the inbox.
///
/// It is possible that delivery fails because the target instance is temporarily unreachable. In
/// this case the task is scheduled for retry after a certain waiting time. For each task delivery
/// is retried up to 3 times after the initial attempt. The retry intervals are as follows:
/// - one minute, for service restart
/// - one hour, for instance maintenance
/// - 2.5 days, for major incident with rebuild from backup
///
/// In case [crate::config::FederationConfigBuilder::debug] is enabled, no background thread is used but activities
/// are sent directly on the foreground. This makes it easier to catch delivery errors and avoids
/// complicated steps to await delivery.
///
/// - `activity`: The activity to be sent, gets converted to json
/// - `private_key`: Private key belonging to the actor who sends the activity, for signing HTTP
///                  signature. Generated with [crate::core::http_signatures::generate_actor_keypair].
/// - `inboxes`: List of actor inboxes that should receive the activity. Should be built by calling
///              [crate::traits::Actor::shared_inbox_or_inbox] for each target actor.
///
/// ```
/// # use activitypub_federation::config::FederationConfig;
/// # use activitypub_federation::core::activity_queue::send_activity;
/// # use activitypub_federation::core::http_signatures::generate_actor_keypair;
/// # use activitypub_federation::traits::Actor;
/// # use activitypub_federation::core::object_id::ObjectId;
/// # use activitypub_federation::traits::tests::{DB_USER, DbConnection, Follow};
/// # let _ = actix_rt::System::new();
/// # actix_rt::Runtime::new().unwrap().block_on(async {
/// # let db_connection = DbConnection;
/// # let config = FederationConfig::builder()
/// #     .domain("example.com")
/// #     .app_data(db_connection)
/// #     .build()?;
/// # let data = config.to_request_data();
/// # let recipient = DB_USER.clone();
/// // Each actor has a keypair. Generate it on signup and store it in the database.
/// let keypair = generate_actor_keypair()?;
/// let activity = Follow {
///     actor: ObjectId::new("https://lemmy.ml/u/nutomic")?,
///     object: recipient.apub_id.clone().into(),
///     kind: Default::default(),
///     id: "https://lemmy.ml/activities/321".try_into()?
/// };
/// let inboxes = vec![recipient.shared_inbox_or_inbox()];
/// send_activity(activity, keypair.private_key, inboxes, &data).await?;
/// # Ok::<(), anyhow::Error>(())
/// # }).unwrap()
/// ```
pub async fn send_activity<Activity, Datatype>(
    activity: Activity,
    private_key: String,
    inboxes: Vec<Url>,
    data: &RequestData<Datatype>,
) -> Result<(), <Activity as ActivityHandler>::Error>
where
    Activity: ActivityHandler + Serialize,
    <Activity as ActivityHandler>::Error: From<anyhow::Error> + From<serde_json::Error>,
    Datatype: Clone,
{
    let config = &data.config;
    let actor_id = activity.actor();
    let activity_id = activity.id();
    let activity_serialized = serde_json::to_string_pretty(&activity)?;
    let inboxes: Vec<Url> = inboxes
        .into_iter()
        .unique()
        .filter(|i| !config.is_local_url(i))
        .collect();

    // This field is only optional to make builder work, its always present at this point
    let activity_queue = config
        .activity_queue
        .as_ref()
        .expect("Config has activity queue");
    for inbox in inboxes {
        if config.verify_url_valid(&inbox).await.is_err() {
            continue;
        }

        let message = SendActivityTask {
            actor_id: actor_id.clone(),
            activity_id: activity_id.clone(),
            inbox,
            activity: activity_serialized.clone(),
            private_key: private_key.clone(),
            http_signature_compat: config.http_signature_compat,
        };
        if config.debug {
            let res = do_send(message, &config.client, config.request_timeout).await;
            // Don't fail on error, as we intentionally do some invalid actions in tests, to verify that
            // they are rejected on the receiving side. These errors shouldn't bubble up to make the API
            // call fail. This matches the behaviour in production.
            if let Err(e) = res {
                warn!("{}", e);
            }
        } else {
            activity_queue.queue(message).await?;
            let stats = activity_queue.get_stats().await?;
            info!(
                "Activity queue stats: pending: {}, running: {}, dead (this hour): {}, complete (this hour): {}",
                stats.pending,
                stats.running,
                stats.dead.this_hour(),
                stats.complete.this_hour()
            );
            if stats.running as u64 == config.worker_count {
                warn!("Maximum number of activitypub workers reached. Consider increasing worker count to avoid federation delays");
            }
        }
    }

    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SendActivityTask {
    actor_id: Url,
    activity_id: Url,
    activity: String,
    inbox: Url,
    private_key: String,
    http_signature_compat: bool,
}

impl ActixJob for SendActivityTask {
    type State = QueueState;
    type Future = Pin<Box<dyn Future<Output = Result<(), anyhow::Error>>>>;
    const NAME: &'static str = "SendActivityTask";

    const MAX_RETRIES: MaxRetries = MaxRetries::Count(3);
    /// This gives the following retry intervals:
    /// - 60s (one minute, for service restart)
    /// - 60min (one hour, for instance maintenance)
    /// - 60h (2.5 days, for major incident with rebuild from backup)
    const BACKOFF: Backoff = Backoff::Exponential(60);

    fn run(self, state: Self::State) -> Self::Future {
        Box::pin(async move { do_send(self, &state.client, state.timeout).await })
    }
}

async fn do_send(
    task: SendActivityTask,
    client: &ClientWithMiddleware,
    timeout: Duration,
) -> Result<(), anyhow::Error> {
    debug!("Sending {} to {}", task.activity_id, task.inbox);
    let request_builder = client
        .post(task.inbox.to_string())
        .timeout(timeout)
        .headers(generate_request_headers(&task.inbox));
    let request = sign_request(
        request_builder,
        task.actor_id,
        task.activity,
        task.private_key,
        task.http_signature_compat,
    )
    .await?;
    let response = client.execute(request).await;

    match response {
        Ok(o) if o.status().is_success() => {
            info!(
                "Activity {} delivered successfully to {}",
                task.activity_id, task.inbox
            );
            Ok(())
        }
        Ok(o) if o.status().is_client_error() => {
            info!(
                "Target server {} rejected {}, aborting",
                task.inbox, task.activity_id,
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
        Err(e) => {
            info!(
                "Unable to connect to {}, aborting task {}: {}",
                task.inbox, task.activity_id, e
            );
            Ok(())
        }
    }
}

fn generate_request_headers(inbox_url: &Url) -> HeaderMap {
    let mut host = inbox_url.domain().expect("read inbox domain").to_string();
    if let Some(port) = inbox_url.port() {
        host = format!("{}:{}", host, port);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static(APUB_JSON_CONTENT_TYPE),
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

pub(crate) fn create_activity_queue(
    client: ClientWithMiddleware,
    worker_count: u64,
    request_timeout: Duration,
    debug: bool,
) -> Manager {
    // queue is not used in debug mod, so dont create any workers to avoid log spam
    let worker_count = if debug { 0 } else { worker_count };

    // Configure and start our workers
    WorkerConfig::new_managed(Storage::new(ActixTimer), move |_| QueueState {
        client: client.clone(),
        timeout: request_timeout,
    })
    .register::<SendActivityTask>()
    .set_worker_count("default", worker_count)
    .start()
}

#[derive(Clone)]
struct QueueState {
    client: ClientWithMiddleware,
    timeout: Duration,
}
