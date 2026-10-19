use std::{env, fs, time::Instant};

use promtuz_avif::{DecodeLimits, Decoder};

const LIMITS: DecodeLimits =
    DecodeLimits { max_pixels: 32_000_000, max_edge: 16_384, max_frames: 600 };

struct Arguments {
    input: String,
    repeats: usize,
    device: String,
    thermal: Option<String>,
    cancellation_latency_ms: Option<u64>,
}

fn main() -> Result<(), String> {
    let arguments = arguments()?;
    let input = fs::read(&arguments.input).map_err(|error| format!("read input: {error}"))?;
    let started = Instant::now();
    let mut frames = 0_u64;
    for _ in 0..arguments.repeats {
        let mut decoder =
            Decoder::open(&input, LIMITS).map_err(|error| format!("open AVIF: {error:?}"))?;
        while decoder.decode_next().map_err(|error| format!("decode AVIF: {error:?}"))?.is_some() {
            frames += 1;
        }
    }
    let elapsed = started.elapsed();
    let hash = blake3::hash(&input);
    let peak_kib = linux_peak_kib();
    println!(
        concat!(
            "{{\"input\":\"{}\",\"inputHash\":\"{}\",\"inputBytes\":{},",
            "\"repeats\":{},\"decodedFrames\":{},\"elapsedMs\":{},\"peakMemoryKiB\":{},",
            "\"device\":\"{}\",\"build\":\"{}\",\"thermal\":{},\"cancellationLatencyMs\":{}}}"
        ),
        json(&arguments.input),
        hash.to_hex(),
        input.len(),
        arguments.repeats,
        frames,
        elapsed.as_millis(),
        peak_kib.map_or_else(|| "null".into(), |value| value.to_string()),
        json(&arguments.device),
        if cfg!(debug_assertions) { "debug" } else { "release" },
        arguments
            .thermal
            .as_deref()
            .map_or_else(|| "null".into(), |value| format!("\"{}\"", json(value))),
        arguments.cancellation_latency_ms.map_or_else(|| "null".into(), |value| value.to_string()),
    );
    Ok(())
}

fn arguments() -> Result<Arguments, String> {
    let mut values = env::args().skip(1);
    let mut input = None;
    let mut repeats = 1;
    let mut device = format!("{}-{}", env::consts::OS, env::consts::ARCH);
    let mut thermal = None;
    let mut cancellation_latency_ms = None;
    while let Some(flag) = values.next() {
        let value = values.next().ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--input" => input = Some(value),
            "--repeats" => {
                repeats = value.parse().map_err(|_| "--repeats must be positive".to_string())?
            },
            "--device" => device = value,
            "--thermal" => thermal = Some(value),
            "--cancellation-latency-ms" => {
                cancellation_latency_ms =
                    Some(value.parse().map_err(|_| "invalid cancellation latency".to_string())?)
            },
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    if repeats == 0 {
        return Err("--repeats must be positive".into());
    }
    Ok(Arguments { input: input.ok_or("usage: media-bench --input FILE [--repeats N] [--device NAME] [--thermal STATE] [--cancellation-latency-ms N]".to_string())?, repeats, device, thermal, cancellation_latency_ms })
}

fn linux_peak_kib() -> Option<u64> {
    fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:")?.split_whitespace().next()?.parse().ok())
}

fn json(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
