use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use tokio::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    url: String,
    #[arg(long)]
    nsec: String,
    #[arg(long, default_value_t = 600)]
    count: usize,
    #[arg(long, default_value_t = 0.0)]
    rate: f64,
    #[arg(long, default_value_t = 10)]
    tag_every: usize,
    #[arg(long, default_value_t = 1_800_000_000)]
    created_at_base: u64,
    #[arg(long, default_value_t = 100)]
    window_size: usize,
}

fn percentile_duration(sorted: &[Duration], numerator: usize, denominator: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() - 1) * numerator) / denominator;
    sorted[index]
}

fn average_duration(samples: &[Duration]) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }

    Duration::from_secs_f64(
        samples.iter().map(Duration::as_secs_f64).sum::<f64>() / samples.len() as f64,
    )
}

fn stddev_duration(samples: &[Duration], mean: Duration) -> Duration {
    if samples.len() <= 1 {
        return Duration::ZERO;
    }

    let mean_secs = mean.as_secs_f64();
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = sample.as_secs_f64() - mean_secs;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;
    Duration::from_secs_f64(variance.sqrt())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let keys = Keys::parse(&args.nsec).context("parse nsec")?;
    let interval = (args.rate > 0.0).then(|| Duration::from_secs_f64(1.0 / args.rate));

    let (stream, _) = connect_async(&args.url)
        .await
        .with_context(|| format!("connect websocket {}", args.url))?;
    let (mut write, mut read) = stream.split();

    let started = Instant::now();
    let mut next_deadline = started;
    let mut latencies = Vec::with_capacity(args.count);

    for i in 0..args.count {
        if i > 0 {
            if let Some(interval) = interval {
                next_deadline += interval;
                tokio::time::sleep_until(next_deadline).await;
            }
        }

        let tags = if args.tag_every > 0 && i % args.tag_every == 0 {
            vec![Tag::parse(["t", "bench"]).context("parse tag")?]
        } else {
            Vec::new()
        };
        let event = EventBuilder::new(Kind::TextNote, format!("bench event {i}"))
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(args.created_at_base + i as u64))
            .sign_with_keys(&keys)
            .context("build signed event")?;
        let event_id = event.id.to_hex();
        let payload = serde_json::json!(["EVENT", event]).to_string();

        let send_started = Instant::now();
        write
            .send(Message::Text(payload.into()))
            .await
            .context("send EVENT")?;

        loop {
            let message = read
                .next()
                .await
                .ok_or_else(|| anyhow!("relay closed websocket"))?
                .context("read websocket message")?;
            let Message::Text(text) = message else {
                continue;
            };

            let value: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| format!("parse relay reply: {text}"))?;
            let Some(items) = value.as_array() else {
                continue;
            };
            if items.len() < 4 || items[0].as_str() != Some("OK") {
                continue;
            }
            if items[1].as_str() != Some(event_id.as_str()) {
                continue;
            }

            let ok = items[2]
                .as_bool()
                .ok_or_else(|| anyhow!("relay OK status was not a boolean"))?;
            if !ok {
                let message = items[3].as_str().unwrap_or("unknown relay error");
                return Err(anyhow!("relay rejected event {event_id}: {message}"));
            }
            break;
        }

        latencies.push(send_started.elapsed());
    }

    let total = started.elapsed();
    let mut sorted_latencies = latencies.clone();
    sorted_latencies.sort_unstable();
    let average_latency = average_duration(&latencies);
    let stddev_latency = stddev_duration(&latencies, average_latency);
    let achieved_rate = if total.is_zero() {
        f64::INFINITY
    } else {
        args.count as f64 / total.as_secs_f64()
    };

    println!(
        "stream bench: url={} count={} target_rate={:.2} total={:?} achieved_rate={:.2} avg={:?} stddev={:?} min={:?} p50={:?} p95={:?} p99={:?} max={:?}",
        args.url,
        args.count,
        args.rate,
        total,
        achieved_rate,
        average_latency,
        stddev_latency,
        sorted_latencies.first().copied().unwrap_or(Duration::ZERO),
        percentile_duration(&sorted_latencies, 50, 100),
        percentile_duration(&sorted_latencies, 95, 100),
        percentile_duration(&sorted_latencies, 99, 100),
        sorted_latencies.last().copied().unwrap_or(Duration::ZERO),
    );

    let window_size = args.window_size.max(1);
    for (index, window) in latencies.chunks(window_size).enumerate() {
        let mut sorted_window = window.to_vec();
        sorted_window.sort_unstable();
        let start = index * window_size;
        let end = start + window.len();
        println!(
            "window {}-{} avg={:?} stddev={:?} p50={:?} p95={:?}",
            start,
            end,
            average_duration(window),
            stddev_duration(window, average_duration(window)),
            percentile_duration(&sorted_window, 50, 100),
            percentile_duration(&sorted_window, 95, 100),
        );
    }

    if latencies.len() >= 2 {
        let split = latencies.len() / 2;
        let first_half = average_duration(&latencies[..split.max(1)]);
        let second_half = average_duration(&latencies[split..]);
        let delta = second_half.as_secs_f64() - first_half.as_secs_f64();
        println!(
            "trend first_half_avg={:?} second_half_avg={:?} delta={:+.6}s",
            first_half, second_half, delta,
        );
    }

    write.close().await.context("close websocket")?;
    Ok(())
}
