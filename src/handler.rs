use crate::{
    cache,
    config::CONFIG,
    constants::{
        CONNECT_COLOR, DISCONNECT_COLOR, EXCHANGE, JOIN_COLOR, LEAVE_COLOR, QUEUE_SEND,
        READY_COLOR, RESUME_COLOR,
    },
    metrics::{GATEWAY_EVENTS, GUILD_EVENTS, SHARD_EVENTS},
    models::{DeliveryInfo, DeliveryOpcode, PayloadInfo},
    utils::{get_event_flags, log_discord, log_discord_guild},
};

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, BasicPublishOptions},
    types::FieldTable,
    BasicProperties, Channel,
};
use simd_json::{json, ValueAccess};
use tracing::{info, warn};
use twilight_gateway::{Cluster, Event};

pub async fn outgoing(
    conn: &mut redis::aio::Connection,
    cluster: &mut Cluster,
    channel: &mut lapin::Channel,
) {
    let shard_strings: Vec<String> = (0..CONFIG.shards_total).map(|x| x.to_string()).collect();

    let mut events = cluster.some_events(get_event_flags());

    let mut bot_id = None;

    while let Some((shard, event)) = events.next().await {
        let mut old = None;
        let shard = shard as usize;

        if CONFIG.state_enabled {
            if let Event::Ready(data) = &event {
                if bot_id.is_none() {
                    bot_id = Some(data.user.id);
                }
            }

            if let Some(bot_id) = bot_id {
                match cache::update(conn, &event, bot_id).await {
                    Ok(value) => {
                        old = value;
                    }
                    Err(err) => {
                        warn!("[Shard {}] Failed to update state: {:?}", shard, err);
                    }
                }
            }
        }

        match event {
            Event::GatewayHello(data) => {
                info!("[Shard {}] Hello (heartbeat interval: {})", shard, data);
            }
            Event::GatewayInvalidateSession(data) => {
                info!("[Shard {}] Invalid Session (resumable: {})", shard, data);
            }
            Event::Ready(data) => {
                info!("[Shard {}] Ready (session: {})", shard, data.session_id);
                log_discord(cluster, READY_COLOR, format!("[Shard {}] Ready", shard));
                SHARD_EVENTS.with_label_values(&["Ready"]).inc();
            }
            Event::Resumed => {
                if let Ok(info) = cluster.shard(shard as u64).unwrap().info() {
                    info!(
                        "[Shard {}] Resumed (session: {})",
                        shard,
                        info.session_id().unwrap_or_default()
                    );
                } else {
                    info!("[Shard {}] Resumed", shard);
                }
                log_discord(cluster, RESUME_COLOR, format!("[Shard {}] Resumed", shard));
                SHARD_EVENTS.with_label_values(&["Resumed"]).inc();
            }
            Event::ShardConnected(_) => {
                info!("[Shard {}] Connected", shard);
                log_discord(
                    cluster,
                    CONNECT_COLOR,
                    format!("[Shard {}] Connected", shard),
                );
                SHARD_EVENTS.with_label_values(&["Connected"]).inc();
            }
            Event::ShardConnecting(data) => {
                info!("[Shard {}] Connecting (url: {})", shard, data.gateway);
                SHARD_EVENTS.with_label_values(&["Connecting"]).inc();
            }
            Event::ShardDisconnected(data) => {
                if let Some(code) = data.code {
                    let reason = data.reason.unwrap_or_default();
                    if !reason.is_empty() {
                        info!(
                            "[Shard {}] Disconnected (code: {}, reason: {})",
                            shard, code, reason
                        );
                    } else {
                        info!("[Shard {}] Disconnected (code: {})", shard, code);
                    }
                } else {
                    info!("[Shard {}] Disconnected", shard);
                }
                log_discord(
                    cluster,
                    DISCONNECT_COLOR,
                    format!("[Shard {}] Disconnected", shard),
                );
                SHARD_EVENTS.with_label_values(&["Disconnected"]).inc();
            }
            Event::ShardIdentifying(_) => {
                info!("[Shard {}] Identifying", shard);
                SHARD_EVENTS.with_label_values(&["Identifying"]).inc();
            }
            Event::ShardReconnecting(_) => {
                info!("[Shard {}] Reconnecting", shard);
                SHARD_EVENTS.with_label_values(&["Reconnecting"]).inc();
            }
            Event::ShardResuming(data) => {
                info!("[Shard {}] Resuming (sequence: {})", shard, data.seq);
                SHARD_EVENTS.with_label_values(&["Resuming"]).inc();
            }
            Event::ShardPayload(mut data) => {
                match simd_json::from_slice::<PayloadInfo>(data.bytes.as_mut_slice()) {
                    Ok(mut payload) => {
                        if let Some(kind) = payload.t.as_deref() {
                            GATEWAY_EVENTS
                                .with_label_values(&[kind, shard_strings[shard as usize].as_str()])
                                .inc();

                            payload.old = old;

                            match simd_json::to_vec(&payload) {
                                Ok(payload) => {
                                    let result = channel
                                        .basic_publish(
                                            EXCHANGE,
                                            kind,
                                            BasicPublishOptions::default(),
                                            payload,
                                            BasicProperties::default(),
                                        )
                                        .await;

                                    if let Err(err) = result {
                                        warn!(
                                            "[Shard {}] Failed to publish event: {:?}",
                                            shard, err
                                        );
                                    }
                                }
                                Err(err) => {
                                    warn!(
                                        "[Shard {}] Failed to serialize payload: {:?}",
                                        shard, err
                                    );
                                }
                            }
                        }
                    }
                    Err(err) => {
                        warn!("[Shard {}] Could not decode payload: {:?}", shard, err);
                    }
                }
            }
            Event::GuildCreate(data) => {
                if old.is_none() {
                    GUILD_EVENTS.with_label_values(&["Join"]).inc();
                    log_discord_guild(
                        cluster,
                        JOIN_COLOR,
                        "Guild Join",
                        format!("{} ({})", data.name, data.id),
                    );
                }
            }
            Event::GuildDelete(data) => {
                if !data.unavailable {
                    GUILD_EVENTS.with_label_values(&["Leave"]).inc();
                    let old_data = old.unwrap_or(json!({}));
                    let guild = old_data.as_object().unwrap();
                    log_discord_guild(
                        cluster,
                        LEAVE_COLOR,
                        "Guild Leave",
                        format!(
                            "{} ({})",
                            guild
                                .get("name")
                                .map(|name| name.as_str().unwrap())
                                .unwrap_or("Unknown"),
                            guild
                                .get("id")
                                .map(|id| id.as_str().unwrap())
                                .unwrap_or("0")
                        ),
                    );
                }
            }
            _ => {}
        }
    }
}

pub async fn incoming(clusters: &mut [Cluster], channel: &mut Channel) {
    let mut consumer = match channel
        .basic_consume(
            QUEUE_SEND,
            "",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
    {
        Ok(channel) => channel,
        Err(err) => {
            warn!("Failed to consume delivery channel: {:?}", err);
            return;
        }
    };

    while let Some(message) = consumer.next().await {
        match message {
            Ok((channel, mut delivery)) => {
                let _ = channel
                    .basic_ack(delivery.delivery_tag, BasicAckOptions::default())
                    .await;
                match simd_json::from_slice::<DeliveryInfo>(delivery.data.as_mut_slice()) {
                    Ok(payload) => {
                        let cluster = clusters
                            .iter()
                            .find(|cluster| cluster.shard(payload.shard).is_some());
                        if let Some(cluster) = cluster {
                            match payload.op {
                                DeliveryOpcode::Send => {
                                    if let Err(err) = cluster
                                        .command(payload.shard, &payload.data.unwrap_or_default())
                                        .await
                                    {
                                        warn!("Failed to send gateway command: {:?}", err);
                                    }
                                }
                                DeliveryOpcode::Reconnect => {
                                    info!("Shutting down shard {}", payload.shard);
                                    cluster.shard(payload.shard).unwrap().shutdown();
                                }
                            }
                        } else {
                            warn!("Delivery received for invalid shard: {}", payload.shard)
                        }
                    }
                    Err(err) => {
                        warn!("Failed to deserialize payload: {:?}", err);
                    }
                }
            }
            Err(err) => {
                warn!("Failed to consume delivery: {:?}", err);
            }
        }
    }
}
