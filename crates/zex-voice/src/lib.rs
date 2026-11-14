//! 录音与 WAV 编码。
//!
//! 只为语音输入服务：按住说话 → 拿到一段 16kHz 单声道 PCM →
//! 交给 ASR。不做播放、不做音效处理。
//!
//! 采样率统一压到 16000 Hz：DeepSeek ASR 按音频大小限制 8 MiB，
//! 48kHz 只能录 87 秒就超了，16kHz 能撑满 120 秒的上限。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use zex_core::{Error, Result};

/// ASR 期望的采样率。
pub const TARGET_RATE: u32 = 16_000;

/// 一次录音。
pub struct Recorder {
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    source_rate: u32,
    /// 后台录音流，drop 时停止。
    _stream: cpal::Stream,
}

impl Recorder {
    /// 开始录音。
    pub fn start() -> Result<Self> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or_else(|| {
            Error::Other("找不到可用的麦克风".into())
        })?;
        let config = device
            .default_input_config()
            .map_err(|e| Error::Other(format!("读取麦克风配置失败: {e}")))?;
        let source_rate = config.sample_rate().0;
        let channels = config.channels() as usize;

        let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let sink = samples.clone();
        let err_fn = |e| tracing::warn!(error = %e, "录音流出错");

        // 只取左声道：ASR 也只识别左声道，先丢掉右声道省带宽
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                let stop = stop.clone();
                device.build_input_stream(
                    &config.into(),
                    move |data: &[f32], _| {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let mut buf = sink.lock().expect("录音缓冲中毒");
                        buf.extend(data.chunks(channels).map(|c| c[0]));
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let stop = stop.clone();
                device.build_input_stream(
                    &config.into(),
                    move |data: &[i16], _| {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let mut buf = sink.lock().expect("录音缓冲中毒");
                        buf.extend(data.chunks(channels).map(|c| c[0] as f32 / 32768.0));
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                return Err(Error::Other(format!(
                    "不支持的采样格式 {other:?}，暂时只认 f32/i16"
                )));
            }
        }
        .map_err(|e| Error::Other(format!("无法打开麦克风: {e}")))?;

        stream
            .play()
            .map_err(|e| Error::Other(format!("启动录音失败: {e}")))?;

        Ok(Recorder {
            samples,
            stop,
            source_rate,
            _stream: stream,
        })
    }

    /// 停止并返回 16 位 PCM 单声道 WAV。
    pub fn stop(self) -> Vec<u8> {
        self.stop.store(true, Ordering::Relaxed);
        // 给回调最后一点时间收尾，避免尾音被截
        std::thread::sleep(std::time::Duration::from_millis(60));
        let raw = self.samples.lock().expect("录音缓冲中毒").clone();
        drop(self._stream);

        let pcm = resample_to_16k(&raw, self.source_rate);
        encode_wav(&pcm, TARGET_RATE)
    }
}

/// 后台上录音的句柄。
///
/// `cpal::Stream` 在 Linux 上不是 `Send`，没法直接放进界面状态。
/// 所以录音跑在独立线程里，主线程只拿这个句柄收发。
pub struct Recording {
    stop_tx: std::sync::mpsc::Sender<()>,
    done_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    started: std::time::Instant,
}

impl Recording {
    /// 开一个后台录音线程。
    pub fn start() -> Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::Builder::new()
            .name("zex-voice".into())
            .spawn(move || {
                // Recorder 留在这个线程里，不跨线程移动
                let rec = match Recorder::start() {
                    Ok(r) => {
                        let _ = ready_tx.send(Ok(()));
                        r
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // 一直等到界面叫停
                let _ = stop_rx.recv();
                let _ = done_tx.send(rec.stop());
            })
            .map_err(|e| Error::Other(format!("无法创建录音线程: {e}")))?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Recording {
                stop_tx,
                done_rx,
                started: std::time::Instant::now(),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::Other("打开麦克风超时".into())),
        }
    }

    /// 已经录了多久。
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// 停止并拿到 WAV。最多等 5 秒收尾。
    pub fn stop(self) -> Result<Vec<u8>> {
        let _ = self.stop_tx.send(());
        self.done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| Error::Other("录音线程没有返回数据".into()))
    }
}

/// 线性降采样到 16kHz，并转成 16 位有符号。
///
/// 语音识别对音质不敏感，线性插值足够；重采样库那是给
/// 音乐场景用的，引进来不划算。
fn resample_to_16k(src: &[f32], src_rate: u32) -> Vec<i16> {
    if src.is_empty() || src_rate == 0 {
        return Vec::new();
    }
    if src_rate == TARGET_RATE {
        return src.iter().map(|s| to_i16(*s)).collect();
    }
    let ratio = src_rate as f64 / TARGET_RATE as f64;
    let out_len = (src.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos as usize;
        let frac = pos - idx as f64;
        let a = src[idx.min(src.len() - 1)];
        let b = src[(idx + 1).min(src.len() - 1)];
        out.push(to_i16(a + (b - a) * frac as f32));
    }
    out
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// 拼一个 44 字节头的单声道 16 位 WAV。
fn encode_wav(pcm: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt 块长度
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // 字节率
    out.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    out.extend_from_slice(&16u16.to_le_bytes()); // 位深
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_well_formed() {
        let pcm = vec![0i16; 100];
        let wav = encode_wav(&pcm, TARGET_RATE);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + 200);
        // data 长度字段
        let dl = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(dl, 200);
    }

    #[test]
    fn resample_keeps_duration() {
        // 48k 的 1 秒 → 16k 的 1 秒
        let src = vec![0.0f32; 48_000];
        let out = resample_to_16k(&src, 48_000);
        assert!((out.len() as i64 - 16_000).abs() <= 2, "got {}", out.len());
    }

    #[test]
    fn resample_passthrough_at_16k() {
        let src = vec![0.5f32; 100];
        let out = resample_to_16k(&src, 16_000);
        assert_eq!(out.len(), 100);
        assert!(out[0] > 16_000);
    }

    #[test]
    fn empty_input_is_safe() {
        assert!(resample_to_16k(&[], 48_000).is_empty());
        let wav = encode_wav(&[], TARGET_RATE);
        assert_eq!(wav.len(), 44);
    }
}

#[cfg(test)]
mod recording_tests {
    use super::*;

    /// 没有麦克风的环境（CI）不该 panic，只该返回 Err。
    #[test]
    fn start_without_device_returns_error_not_panic() {
        match Recording::start() {
            Ok(_) => {}      // 有麦克风的机器，正常
            Err(e) => {
                let msg = e.to_string();
                assert!(!msg.is_empty(), "错误信息不该是空的");
            }
        }
    }
}
