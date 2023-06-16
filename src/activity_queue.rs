//! Queue for signing and sending outgoing activities with retry
//!
#![doc = include_str!("../docs/09_sending_activities.md")]

use crate::{
    config::Data,
    error::Error,
    http_signatures::sign_request,
    reqwest_shim::ResponseExt,
    traits::{ActivityHandler, Actor},
    FEDERATION_CONTENT_TYPE,
};
use anyhow::anyhow;

use bytes::Bytes;
use futures_core::Future;
use http::{header::HeaderName, HeaderMap, HeaderValue};
use httpdate::fmt_http_date;
use itertools::Itertools;
use openssl::pkey::{PKey, Private};
use reqwest::Request;
use reqwest_middleware::ClientWithMiddleware;
use serde::Serialize;
use std::{
    fmt::{Debug, Display},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tracing::{debug, info, warn};
use url::Url;

/// Send a new activity to the given inboxes
///
/// - `activity`: The activity to be sent, gets converted to json
/// - `private_key`: Private key belonging to the actor who sends the activity, for signing HTTP
///                  signature. Generated with [crate::http_signatures::generate_actor_keypair].
/// - `inboxes`: List of remote actor inboxes that should receive the activity. Ignores local actor
///              inboxes. Should be built by calling [crate::traits::Actor::shared_inbox_or_inbox]
///              for each target actor.
pub async fn send_activity<Activity, Datatype, ActorType>(
    activity: Activity,
    actor: &ActorType,
    inboxes: Vec<Url>,
    data: &Data<Datatype>,
) -> Result<(), <Activity as ActivityHandler>::Error>
where
    Activity: ActivityHandler + Serialize,
    <Activity as ActivityHandler>::Error: From<anyhow::Error> + From<serde_json::Error>,
    Datatype: Clone,
    ActorType: Actor,
{
    let config = &data.config;
    let actor_id = activity.actor();
    let activity_id = activity.id();
    let activity_serialized: Bytes = serde_json::to_vec(&activity)?.into();
    let private_key_pem = actor
        .private_key_pem()
        .ok_or_else(|| anyhow!("Actor {actor_id} does not contain a private key for signing"))?;

    // This is a mostly expensive blocking call, we don't want to tie up other tasks while this is happening
    let private_key = tokio::task::block_in_place(|| {
        PKey::private_key_from_pem(private_key_pem.as_bytes())
            .map_err(|err| anyhow!("Could not create private key from PEM data:{err}"))
    })?;

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

        activity_queue.queue(message).await?;
        let stats = activity_queue.get_stats();
        let running = stats.running.load(Ordering::Relaxed);
        let stats_fmt = format!(
            "Activity queue stats: pending: {}, running: {}, dead: {}, complete: {}",
            stats.pending.load(Ordering::Relaxed),
            running,
            stats.dead_last_hour.load(Ordering::Relaxed),
            stats.completed_last_hour.load(Ordering::Relaxed),
        );
        if running == config.worker_count {
            warn!("Reached max number of send activity workers ({}). Consider increasing worker count to avoid federation delays", config.worker_count);
            warn!(stats_fmt);
        } else {
            info!(stats_fmt);
        }
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct SendActivityTask {
    actor_id: Url,
    activity_id: Url,
    activity: Bytes,
    inbox: Url,
    private_key: PKey<Private>,
    http_signature_compat: bool,
}

async fn sign_and_send(
    task: &SendActivityTask,
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
        &task.actor_id,
        task.activity.clone(),
        task.private_key.clone(),
        task.http_signature_compat,
    )
    .await?;

    send(task, client, request).await
}

async fn send(
    task: &SendActivityTask,
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
        Err(e) => {
            warn!(
                "Unable to connect to {}, aborting task {}: {}",
                task.inbox, task.activity_id, e
            );
            Ok(())
        }
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

/// A simple activity queue which spawns tokio workers to send out requests
/// When creating a queue, it will spawn a task per worker thread
/// Uses an unbounded mpsc queue for communication (i.e, all messages are in memory)
pub(crate) struct ActivityQueue {
    // Our "background" tasks
    senders: Vec<UnboundedSender<SendActivityTask>>,
    handles: Vec<JoinHandle<()>>,
    reset_handle: JoinHandle<()>,
    // Round robin of the sender list
    last_sender_idx: AtomicUsize,
    // Stats shared between the queue and workers
    stats: Arc<Stats>,
}

/// Simple stat counter to show where we're up to with sending messages
/// This is a lock-free way to share things between tasks
/// When reading these values it's possible (but extremely unlikely) to get stale data if a worker task is in the middle of transitioning
#[derive(Default)]
struct Stats {
    pending: AtomicUsize,
    running: AtomicUsize,
    dead_last_hour: AtomicUsize,
    completed_last_hour: AtomicUsize,
}

#[derive(Clone, Copy)]
struct RetryStrategy {
    /// Amount of time in seconds to back off
    backoff: usize,
    /// Amount of times to retry
    retries: usize,
}

/// A tokio spawned worker which is responsible for submitting requests to federated servers
async fn worker(
    client: ClientWithMiddleware,
    timeout: Duration,
    mut receiver: UnboundedReceiver<SendActivityTask>,
    stats: Arc<Stats>,
    strategy: RetryStrategy,
) {
    while let Some(message) = receiver.recv().await {
        stats.pending.fetch_sub(1, Ordering::Relaxed);
        stats.running.fetch_add(1, Ordering::Relaxed);

        let outcome = retry(|| sign_and_send(&message, &client, timeout), strategy).await;

        // "Running" has finished, check the outcome
        stats.running.fetch_sub(1, Ordering::Relaxed);

        match outcome {
            Ok(_) => {
                stats.completed_last_hour.fetch_add(1, Ordering::Relaxed);
            }
            Err(_err) => {
                stats.dead_last_hour.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl ActivityQueue {
    fn new(
        client: ClientWithMiddleware,
        worker_count: usize,
        timeout: Duration,
        strategy: RetryStrategy,
    ) -> Self {
        // Keep a vec of senders to send our messages to
        let mut senders = Vec::with_capacity(worker_count);
        let mut handles = Vec::with_capacity(worker_count);

        let stats: Arc<Stats> = Default::default();

        // This task clears the dead/completed stats every hour
        let hour_stats = stats.clone();
        let reset_handle = tokio::spawn(async move {
            let duration = Duration::from_secs(3600);
            loop {
                tokio::time::sleep(duration).await;
                hour_stats.completed_last_hour.store(0, Ordering::Relaxed);
                hour_stats.dead_last_hour.store(0, Ordering::Relaxed);
            }
        });

        // Spawn our workers
        for _ in 0..worker_count {
            let (sender, receiver) = unbounded_channel();
            handles.push(tokio::spawn(worker(
                client.clone(),
                timeout,
                receiver,
                stats.clone(),
                strategy,
            )));
            senders.push(sender);
        }

        Self {
            senders,
            handles,
            reset_handle,
            last_sender_idx: AtomicUsize::new(0),
            stats,
        }
    }
    async fn queue(&self, message: SendActivityTask) -> Result<(), anyhow::Error> {
        // really basic round-robin to our workers, we just do mod on the len of senders
        let idx_to_send = self.last_sender_idx.fetch_add(1, Ordering::Relaxed) % self.senders.len();

        // Set a queue to pending
        self.stats.pending.fetch_add(1, Ordering::Relaxed);

        // Send to one of our workers
        self.senders[idx_to_send].send(message)?;

        Ok(())
    }

    fn get_stats(&self) -> &Stats {
        &self.stats
    }

    #[allow(unused)]
    // Drops all the senders and shuts down the workers
    async fn shutdown(self) -> Result<Stats, anyhow::Error> {
        drop(self.senders);

        // stop the reset counter task
        self.reset_handle.abort();
        self.reset_handle.await.ok();

        for handle in self.handles {
            handle.await?;
        }

        Arc::try_unwrap(self.stats).map_err(|_| anyhow!("Could not retrieve stats"))
    }
}

/// Creates an activity queue using tokio spawned tasks
/// Note: requires a tokio runtime
pub(crate) fn create_activity_queue(
    client: ClientWithMiddleware,
    worker_count: usize,
    request_timeout: Duration,
) -> ActivityQueue {
    assert!(
        worker_count > 0,
        "worker count needs to be greater than zero"
    );
    /// We need to retry activity sending in case the target instances is temporarily unreachable.
    /// In this case, the task is stored and resent when the instance is hopefully back up. This
    /// list shows the retry intervals, and which events of the target instance can be covered:
    /// - 60s (one minute, service restart)
    /// - 60min (one hour, instance maintenance)
    /// - 60h (2.5 days, major incident with rebuild from backup)
    const MAX_RETRIES: usize = 3;
    const BACKOFF: usize = 60;

    ActivityQueue::new(
        client,
        worker_count,
        request_timeout,
        RetryStrategy {
            backoff: BACKOFF,
            retries: MAX_RETRIES,
        },
    )
}

/// Retries a future action factory function up to `amount` times with an exponential backoff timer between tries
async fn retry<T, E: Display, F: Future<Output = Result<T, E>>, A: FnMut() -> F>(
    mut action: A,
    strategy: RetryStrategy,
) -> Result<T, E> {
    let mut count = 0;

    loop {
        match action().await {
            Ok(val) => return Ok(val),
            Err(err) => {
                if count < strategy.retries {
                    count += 1;

                    let sleep_amt = strategy.backoff.pow(count as u32) as u64;
                    let sleep_dur = Duration::from_secs(sleep_amt);
                    warn!("{err}.  Sleeping for {sleep_dur:?} and trying again");
                    tokio::time::sleep(sleep_dur).await;
                    continue;
                } else {
                    return Err(err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::State;
    use bytes::Bytes;
    use http::StatusCode;
    use std::time::Instant;

    use crate::http_signatures::generate_actor_keypair;

    use super::*;

    #[allow(unused)]
    // This will periodically send back internal errors to test the retry
    async fn dodgy_handler(
        State(state): State<Arc<AtomicUsize>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<(), StatusCode> {
        debug!("Headers:{:?}", headers);
        debug!("Body len:{}", body.len());

        if state.fetch_add(1, Ordering::Relaxed) % 20 == 0 {
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Ok(())
    }

    async fn test_server() {
        use axum::{routing::post, Router};

        // We should break every now and then ;)
        let state = Arc::new(AtomicUsize::new(0));

        let app = Router::new()
            .route("/", post(dodgy_handler))
            .with_state(state);

        axum::Server::bind(&"0.0.0.0:8001".parse().unwrap())
            .serve(app.into_make_service())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    // Queues 10_000 messages and then asserts that the worker runs them
    async fn test_activity_queue_workers() {
        let num_workers = 64;
        let num_messages: usize = 100;

        tokio::spawn(test_server());

        /*
        // uncomment for debug logs & stats
        use tracing::log::LevelFilter;

        env_logger::builder()
            .filter_level(LevelFilter::Warn)
            .filter_module("activitypub_federation", LevelFilter::Info)
            .format_timestamp(None)
            .init();
        */

        let activity_queue = ActivityQueue::new(
            reqwest::Client::default().into(),
            num_workers,
            Duration::from_secs(10),
            RetryStrategy {
                backoff: 1,
                retries: 3,
            },
        );

        let keypair = generate_actor_keypair().unwrap();

        let message = SendActivityTask {
            actor_id: "http://localhost:8001".parse().unwrap(),
            activity_id: "http://localhost:8001/activity".parse().unwrap(),
            activity: "{}".into(),
            inbox: "http://localhost:8001".parse().unwrap(),
            private_key: keypair.private_key().unwrap(),
            http_signature_compat: true,
        };

        let start = Instant::now();

        for _ in 0..num_messages {
            activity_queue.queue(message.clone()).await.unwrap();
        }

        info!("Queue Sent: {:?}", start.elapsed());

        let stats = activity_queue.shutdown().await.unwrap();

        info!(
            "Queue Finished.  Num msgs: {}, Time {:?}, msg/s: {:0.0}",
            num_messages,
            start.elapsed(),
            num_messages as f64 / start.elapsed().as_secs_f64()
        );

        assert_eq!(
            stats.completed_last_hour.load(Ordering::Relaxed),
            num_messages
        );
    }
}
