use crate::{audio, console_log, languages::LANGUAGES};

use anyhow::Error as E;
use rand::{distributions::Distribution, rngs::StdRng, SeedableRng};
use serde::{Deserialize, Serialize};

use candle_core::{safetensors::Load, Device, IndexOp, Tensor, D};
use candle_nn::{ops::softmax, VarBuilder};
pub use candle_transformers::models::whisper::{self as m, Config};
use tokenizers::Tokenizer;

#[derive(Serialize, Deserialize)]
pub struct ModelData {
    pub weights: Vec<u8>,
    pub tokenizer: Vec<u8>,
    pub mel_filters: Vec<u8>,
    pub config: Vec<u8>,
    pub quantized: bool,
    pub timestamps: bool,
    pub is_multilingual: bool,
    pub language: Option<String>,
    pub task: Option<String>,
}

pub enum Model {
    Normal(m::model::Whisper),
    Quantized(m::quantized_model::Whisper),
}
impl Model {
    pub fn config(&self) -> &Config {
        match self {
            Self::Normal(m) => &m.config,
            Self::Quantized(m) => &m.config,
        }
    }

    pub fn encoder_forward(&mut self, x: &Tensor, flush: bool) -> candle_core::Result<Tensor> {
        match self {
            Self::Normal(m) => m.encoder.forward(x, flush),
            Self::Quantized(m) => m.encoder.forward(x, flush),
        }
    }

    pub fn decoder_forward(
        &mut self,
        x: &Tensor,
        xa: &Tensor,
        flush: bool,
    ) -> candle_core::Result<Tensor> {
        match self {
            Self::Normal(m) => m.decoder.forward(x, xa, flush),
            Self::Quantized(m) => m.decoder.forward(x, xa, flush),
        }
    }

    pub fn decoder_final_linear(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        match self {
            Self::Normal(m) => m.decoder.final_linear(x),
            Self::Quantized(m) => m.decoder.final_linear(x),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodingResult {
    pub tokens: Vec<u32>,
    pub text: String,
    pub avg_logprob: f64,
    pub no_speech_prob: f64,
    temperature: f64,
    compression_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub duration: f64,
    pub dr: DecodingResult,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum Task {
    Transcribe,
    Translate,
}

pub struct Decoder {
    model: Model,
    rng: rand::rngs::StdRng,
    task: Option<Task>,
    language: Option<String>,
    is_multilingual: bool,
    mel_filters: Vec<f32>,
    timestamps: bool,
    tokenizer: Tokenizer,
    suppress_tokens: Tensor,
    sot_token: u32,
    transcribe_token: u32,
    translate_token: u32,
    eot_token: u32,
    no_speech_token: u32,
    no_timestamps_token: u32,
}

impl Decoder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        model: Model,
        tokenizer: Tokenizer,
        mel_filters: Vec<f32>,
        device: &Device,
        task: Option<Task>,
        language: Option<String>,
        is_multilingual: bool,
        timestamps: bool,
    ) -> anyhow::Result<Self> {
        let suppress_tokens: Vec<f32> = (0..model.config().vocab_size as u32)
            .map(|i| {
                if model.config().suppress_tokens.contains(&i) {
                    f32::NEG_INFINITY
                } else {
                    0f32
                }
            })
            .collect();
        let no_timestamps_token = token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;
        let suppress_tokens = Tensor::new(suppress_tokens.as_slice(), device)?;
        let sot_token = token_id(&tokenizer, m::SOT_TOKEN)?;
        let transcribe_token = token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let translate_token = token_id(&tokenizer, m::TRANSLATE_TOKEN)?;
        let eot_token = token_id(&tokenizer, m::EOT_TOKEN)?;
        let no_speech_token = m::NO_SPEECH_TOKENS
            .iter()
            .find_map(|token| token_id(&tokenizer, token).ok());
        let no_speech_token = match no_speech_token {
            None => anyhow::bail!("unable to find any non-speech token"),
            Some(n) => n,
        };
        let seed = 299792458;
        Ok(Self {
            model,
            rng: StdRng::seed_from_u64(seed),
            tokenizer,
            mel_filters,
            task,
            timestamps,
            language,
            is_multilingual,
            suppress_tokens,
            sot_token,
            transcribe_token,
            translate_token,
            eot_token,
            no_speech_token,
            no_timestamps_token,
        })
    }

    fn decode(&mut self, mel: &Tensor, t: f64) -> anyhow::Result<DecodingResult> {
        let model = &mut self.model;
        let language_token = match (self.is_multilingual, &self.language) {
            (true, None) => Some(detect(model, &self.tokenizer, mel)?),
            (false, None) => None,
            (true, Some(language)) => {
                match token_id(&self.tokenizer, &format!("<|{:?}|>", self.language)) {
                    Ok(token_id) => Some(token_id),
                    Err(_) => anyhow::bail!("language {language} is not supported"),
                }
            }
            (false, Some(_)) => {
                anyhow::bail!("a language cannot be set for non-multilingual models")
            }
        };

        let audio_features = model.encoder_forward(mel, true)?;
        let sample_len = model.config().max_target_positions / 2;
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = f64::NAN;
        let mut tokens = vec![self.sot_token];
        if let Some(language_token) = language_token {
            tokens.push(language_token);
        }
        match self.task {
            None | Some(Task::Transcribe) => tokens.push(self.transcribe_token),
            Some(Task::Translate) => tokens.push(self.translate_token),
        }
        if !self.timestamps {
            tokens.push(self.no_timestamps_token);
        }
        for i in 0..sample_len {
            let tokens_t = Tensor::new(tokens.as_slice(), mel.device())?;
            let tokens_t = tokens_t.unsqueeze(0)?;
            let ys = model.decoder_forward(&tokens_t, &audio_features, i == 0)?;

            if i == 0 {
                let logits = model.decoder_final_linear(&ys.i(..1)?)?.i(0)?.i(0)?;
                no_speech_prob = softmax(&logits, 0)?
                    .i(self.no_speech_token as usize)?
                    .to_scalar::<f32>()? as f64;
            }

            let (_, seq_len, _) = ys.dims3()?;
            let logits = model
                .decoder_final_linear(&ys.i((..1, seq_len - 1..))?)?
                .i(0)?
                .i(0)?;

            let logits = logits.broadcast_add(&self.suppress_tokens)?;
            let next_token = if t > 0f64 {
                let prs = softmax(&(&logits / t)?, 0)?;
                let logits_v: Vec<f32> = prs.to_vec1()?;
                let distr = rand::distributions::WeightedIndex::new(&logits_v)?;
                distr.sample(&mut self.rng) as u32
            } else {
                let logits_v: Vec<f32> = logits.to_vec1()?;
                logits_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, u), (_, v)| u.total_cmp(v))
                    .map(|(i, _)| i as u32)
                    .unwrap()
            };
            tokens.push(next_token);
            let prob = softmax(&logits, candle_core::D::Minus1)?
                .i(next_token as usize)?
                .to_scalar::<f32>()? as f64;
            if next_token == self.eot_token || tokens.len() > model.config().max_target_positions {
                break;
            }
            sum_logprob += prob.ln();
        }
        let text = self.tokenizer.decode(&tokens, true).map_err(E::msg)?;
        let avg_logprob = sum_logprob / tokens.len() as f64;

        Ok(DecodingResult {
            tokens,
            text,
            avg_logprob,
            no_speech_prob,
            temperature: t,
            compression_ratio: f64::NAN,
        })
    }

    fn decode_with_fallback(&mut self, segment: &Tensor) -> anyhow::Result<DecodingResult> {
        for (i, &t) in m::TEMPERATURES.iter().enumerate() {
            let dr: Result<DecodingResult, _> = self.decode(segment, t);
            if i == m::TEMPERATURES.len() - 1 {
                return dr;
            }
            match dr {
                Ok(dr) => {
                    let needs_fallback = dr.compression_ratio > m::COMPRESSION_RATIO_THRESHOLD
                        || dr.avg_logprob < m::LOGPROB_THRESHOLD;
                    if !needs_fallback || dr.no_speech_prob > m::NO_SPEECH_THRESHOLD {
                        return Ok(dr);
                    }
                }
                Err(err) => {
                    console_log!("[RUST]: Error running at {t}: {err}")
                }
            }
        }
        unreachable!()
    }

    fn run(&mut self, mel: &Tensor) -> anyhow::Result<Vec<Segment>> {
        let (_, _, content_frames) = mel.dims3()?;
        let mut seek = 0;
        let mut segments = vec![];
        while seek < content_frames {
            let time_offset = (seek * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;
            let segment_size = usize::min(content_frames - seek, m::N_FRAMES);
            let mel_segment = mel.narrow(2, seek, segment_size)?;
            let segment_duration = (segment_size * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;
            let dr = self.decode_with_fallback(&mel_segment)?;
            seek += segment_size;
            if dr.no_speech_prob > m::NO_SPEECH_THRESHOLD && dr.avg_logprob < m::LOGPROB_THRESHOLD {
                console_log!("[RUST]: skipping {seek} {dr:?}");
                continue;
            }
            let segment = Segment {
                start: time_offset,
                duration: segment_duration,
                dr,
            };
            segments.push(segment)
        }
        Ok(segments)
    }

    pub fn load(md: ModelData) -> anyhow::Result<Self> {
        let device = Device::Cpu;
        let tokenizer = Tokenizer::from_bytes(&md.tokenizer).map_err(E::msg)?;
        let mel_filters = safetensors::tensor::SafeTensors::deserialize(&md.mel_filters)?;
        let mel_filters = mel_filters.tensor("mel_80")?.load(&device)?;
        console_log!("[RUST]: mel_filters shape '{:?}'", mel_filters.shape());

        let mel_filters = mel_filters.flatten_all()?.to_vec1::<f32>()?;
        let config: Config = serde_json::from_slice(&md.config)?;
        let model = if md.quantized {
            let vb = candle_transformers::quantized_var_builder::VarBuilder::from_gguf_buffer(
                &md.weights,
                &device,
            )?;
            Model::Quantized(m::quantized_model::Whisper::load(&vb, config)?)
        } else {
            let vb = VarBuilder::from_buffered_safetensors(md.weights, m::DTYPE, &device)?;
            Model::Normal(m::model::Whisper::load(&vb, config)?)
        };
        console_log!("[RUST]: model loaded");

        let task = match md.task.as_deref() {
            Some("translate") => Some(Task::Translate),
            _ => Some(Task::Transcribe),
        };
        let decoder = Self::new(
            model,
            tokenizer,
            mel_filters,
            &device,
            task,
            md.language,
            md.is_multilingual,
            md.timestamps,
        )?;
        Ok(decoder)
    }

    pub fn convert_and_run(&mut self, wav_input: &[u8]) -> anyhow::Result<Vec<Segment>> {
        let device = Device::Cpu;
        let mut wav_input = std::io::Cursor::new(wav_input);
        let wav_reader = hound::WavReader::new(&mut wav_input)?;
        let spec = wav_reader.spec();
        console_log!("[RUST]: wav data: {spec:?}");

        if spec.sample_rate != m::SAMPLE_RATE as u32 {
            anyhow::bail!("wav file must have a {} sampling rate", m::SAMPLE_RATE);
        }
        let mut data = wav_reader.into_samples::<i16>().collect::<Vec<_>>();
        data.truncate(data.len() / spec.channels as usize);
        let mut pcm_data = Vec::with_capacity(data.len());
        for d in data.into_iter() {
            let d = d?;
            pcm_data.push(d as f32 / 32768.)
        }
        console_log!("[RUST]: pcm data loaded {}", pcm_data.len());

        let mel = audio::pcm_to_mel(self.model.config(), &pcm_data, &self.mel_filters)?;
        let mel_len = mel.len();
        let n_mels = self.model.config().num_mel_bins;
        let mel = Tensor::from_vec(mel, (1, n_mels, mel_len / n_mels), &device)?;
        console_log!("[RUST]: loaded mel: {:?}", mel.dims());
        let segments = self.run(&mel)?;
        Ok(segments)
    }
}

pub fn detect(model: &mut Model, tokenizer: &Tokenizer, mel: &Tensor) -> Result<u32, E> {
    let (_bsize, _, seq_len) = mel.dims3()?;
    let mel = mel.narrow(
        2,
        0,
        usize::min(seq_len, model.config().max_source_positions),
    )?;
    let device = mel.device();
    let language_token_ids = LANGUAGES
        .iter()
        .map(|(t, _)| token_id(tokenizer, &format!("<|{t}|>")))
        .map(|e| e.map_err(E::msg))
        .collect::<Result<Vec<_>, E>>()?;
    let sot_token = token_id(tokenizer, m::SOT_TOKEN)?;
    let audio_features = model.encoder_forward(&mel, true)?;
    let tokens = Tensor::new(&[[sot_token]], device)?;
    let language_token_ids = Tensor::new(language_token_ids.as_slice(), device)?;
    let ys = model.decoder_forward(&tokens, &audio_features, true)?;
    let logits = model.decoder_final_linear(&ys.i(..1)?)?.i(0)?.i(0)?;
    let logits = logits.index_select(&language_token_ids, 0)?;
    let probs = candle_nn::ops::softmax(&logits, D::Minus1)?;
    let probs = probs.to_vec1::<f32>()?;
    let mut probs = LANGUAGES.iter().zip(probs.iter()).collect::<Vec<_>>();
    probs.sort_by(|(_, p1), (_, p2)| p2.total_cmp(p1));
    let token = &format!("<|{}|>", probs[0].0 .0);
    let language = token_id(tokenizer, token)?;
    console_log!("[RUST]: language: {language} {token}");
    Ok(language)
}

pub fn token_id(tokenizer: &Tokenizer, token: &str) -> candle_core::Result<u32> {
    match tokenizer.token_to_id(token) {
        None => candle_core::bail!("no token-id for {token}"),
        Some(id) => Ok(id),
    }
}
