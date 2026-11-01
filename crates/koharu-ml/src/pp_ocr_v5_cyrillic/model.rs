//! PP-OCRv5 mobile East-Slavic text recognizer.
//!
//! Ported from Transformers at
//! https://github.com/huggingface/transformers/blob/5be474e9cc2c92acaa3ef14fe9e35ea9108b06c1/src/transformers/models/pp_ocrv5_mobile_rec/modeling_pp_ocrv5_mobile_rec.py

use std::path::Path;

use anyhow::Result;
use koharu_torch::{
    Kind, Tensor,
    nn::{self, Module, ModuleT},
};

use crate::pp_lcnet_v3::Backbone;

use super::config::Config;

#[derive(Debug)]
pub(crate) struct Model {
    vs: nn::VarStore,
    backbone: Backbone,
    head: Head,
}

impl Model {
    pub(crate) fn new(config: &Config, device: koharu_torch::Device) -> Self {
        let mut vs = nn::VarStore::new(device);
        crate::backend::set_precision(&mut vs);
        let backbone = Backbone::new(
            &(&vs.root() / "model" / "backbone"),
            &config.backbone_config,
        );
        let head = Head::new(&(&vs.root() / "head"), config);
        vs.freeze();
        Self { vs, backbone, head }
    }

    pub(crate) fn load(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.vs.load(path)?;
        crate::backend::set_precision(&mut self.vs);
        Ok(())
    }

    pub(crate) fn forward(&self, pixel_values: &Tensor) -> Tensor {
        let hidden_states = self.backbone.forward(&pixel_values.to_kind(self.vs.kind()));
        let hidden_states =
            hidden_states.avg_pool2d([3, 2], [3, 2], [0, 0], false, true, None::<i64>);
        self.head.forward(&hidden_states)
    }
}

#[derive(Debug)]
struct ConvLayer {
    convolution: nn::Conv2D,
    normalization: nn::BatchNorm,
    activation: String,
}

impl ConvLayer {
    fn new(
        path: &nn::Path<'_>,
        in_channels: i64,
        out_channels: i64,
        kernel_size: [i64; 2],
        activation: &str,
    ) -> Self {
        Self {
            convolution: nn::conv(
                path / "convolution",
                in_channels,
                out_channels,
                kernel_size,
                nn::ConvConfigND {
                    stride: [1, 1],
                    padding: [kernel_size[0] / 2, kernel_size[1] / 2],
                    bias: false,
                    ..Default::default()
                },
            ),
            normalization: nn::batch_norm2d(
                path / "normalization",
                out_channels,
                Default::default(),
            ),
            activation: activation.into(),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let hidden_states = self
            .normalization
            .forward_t(&self.convolution.forward(input), false);
        activate(hidden_states, &self.activation)
    }
}

fn activate(hidden_states: Tensor, activation: &str) -> Tensor {
    match activation {
        "silu" => hidden_states.silu(),
        "relu" => hidden_states.relu(),
        name => panic!("unsupported PP-OCRv5 recognition activation {name}"),
    }
}

#[derive(Debug)]
struct Attention {
    qkv: nn::Linear,
    projection: nn::Linear,
    num_heads: i64,
    head_dim: i64,
    scale: f64,
}

impl Attention {
    fn new(path: &nn::Path<'_>, config: &Config) -> Self {
        let head_dim = config.hidden_size / config.num_attention_heads;
        assert_eq!(head_dim * config.num_attention_heads, config.hidden_size);
        Self {
            qkv: nn::linear(
                path / "qkv",
                config.hidden_size,
                config.hidden_size * 3,
                nn::LinearConfig {
                    bias: config.qkv_bias,
                    ..Default::default()
                },
            ),
            projection: nn::linear(
                path / "projection",
                config.hidden_size,
                config.hidden_size,
                Default::default(),
            ),
            num_heads: config.num_attention_heads,
            head_dim,
            scale: (head_dim as f64).powf(-0.5),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let size = input.size();
        let (batch_size, sequence_length, embed_dim) = (size[0], size[1], size[2]);
        let mixed = self
            .qkv
            .forward(input)
            .view([
                batch_size,
                sequence_length,
                3,
                self.num_heads,
                self.head_dim,
            ])
            .permute([2, 0, 3, 1, 4]);
        let query = mixed.get(0);
        let key = mixed.get(1);
        let value = mixed.get(2);
        let weights = (query.matmul(&key.transpose(-1, -2)) * self.scale).softmax(-1, None::<Kind>);
        let hidden_states = weights.matmul(&value).transpose(1, 2).contiguous().view([
            batch_size,
            sequence_length,
            embed_dim,
        ]);
        self.projection.forward(&hidden_states)
    }
}

#[derive(Debug)]
struct Mlp {
    fc1: nn::Linear,
    fc2: nn::Linear,
    activation: String,
}

impl Mlp {
    fn new(path: &nn::Path<'_>, config: &Config) -> Self {
        let hidden_features = (config.hidden_size as f64 * config.mlp_ratio) as i64;
        Self {
            fc1: nn::linear(
                path / "fc1",
                config.hidden_size,
                hidden_features,
                Default::default(),
            ),
            fc2: nn::linear(
                path / "fc2",
                hidden_features,
                config.hidden_size,
                Default::default(),
            ),
            activation: config.hidden_act.clone(),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        self.fc2
            .forward(&activate(self.fc1.forward(input), &self.activation))
    }
}

#[derive(Debug)]
struct TransformerBlock {
    attention: Attention,
    layer_norm1: nn::LayerNorm,
    mlp: Mlp,
    layer_norm2: nn::LayerNorm,
}

impl TransformerBlock {
    fn new(path: &nn::Path<'_>, config: &Config) -> Self {
        let norm_config = nn::LayerNormConfig {
            eps: config.layer_norm_eps,
            ..Default::default()
        };
        Self {
            attention: Attention::new(&(path / "self_attn"), config),
            layer_norm1: nn::layer_norm(
                path / "layer_norm1",
                vec![config.hidden_size],
                norm_config,
            ),
            mlp: Mlp::new(&(path / "mlp"), config),
            layer_norm2: nn::layer_norm(
                path / "layer_norm2",
                vec![config.hidden_size],
                norm_config,
            ),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let hidden_states = input + self.attention.forward(&self.layer_norm1.forward(input));
        &hidden_states + self.mlp.forward(&self.layer_norm2.forward(&hidden_states))
    }
}

#[derive(Debug)]
struct Encoder {
    conv_block: [ConvLayer; 5],
    transformer_blocks: Vec<TransformerBlock>,
    norm: nn::LayerNorm,
}

impl Encoder {
    fn new(path: &nn::Path<'_>, config: &Config) -> Self {
        let in_channels = config
            .backbone_config
            .block_configs
            .last()
            .and_then(|stage| stage.last())
            .map(|block| {
                crate::pp_lcnet_v3::make_divisible(
                    block.out_channels as f64 * config.backbone_config.scale,
                    config.backbone_config.divisor,
                )
            })
            .expect("PP-OCRv5 recognition config has a final backbone block");
        let inner_channels = in_channels / 8;
        let hidden_size = config.hidden_size;
        let kernel_size = [config.conv_kernel_size[0], config.conv_kernel_size[1]];
        Self {
            conv_block: [
                ConvLayer::new(
                    &(path / "conv_block" / 0),
                    in_channels,
                    inner_channels,
                    kernel_size,
                    &config.hidden_act,
                ),
                ConvLayer::new(
                    &(path / "conv_block" / 1),
                    inner_channels,
                    hidden_size,
                    [1, 1],
                    &config.hidden_act,
                ),
                ConvLayer::new(
                    &(path / "conv_block" / 2),
                    hidden_size,
                    in_channels,
                    [1, 1],
                    &config.hidden_act,
                ),
                ConvLayer::new(
                    &(path / "conv_block" / 3),
                    in_channels * 2,
                    inner_channels,
                    kernel_size,
                    &config.hidden_act,
                ),
                ConvLayer::new(
                    &(path / "conv_block" / 4),
                    inner_channels,
                    hidden_size,
                    [1, 1],
                    &config.hidden_act,
                ),
            ],
            transformer_blocks: (0..config.depth)
                .map(|index| TransformerBlock::new(&(path / "svtr_block" / index), config))
                .collect(),
            norm: nn::layer_norm(
                path / "norm",
                vec![hidden_size],
                nn::LayerNormConfig {
                    eps: config.layer_norm_eps,
                    ..Default::default()
                },
            ),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let residual = input.shallow_clone();
        let mut hidden_states = self.conv_block[1].forward(&self.conv_block[0].forward(input));
        let size = hidden_states.size();
        let (batch_size, channels, height, width) = (size[0], size[1], size[2], size[3]);
        hidden_states = hidden_states.flatten(2, -1).transpose(1, 2);
        for block in &self.transformer_blocks {
            hidden_states = block.forward(&hidden_states);
        }
        hidden_states = self.norm.forward(&hidden_states);
        hidden_states = hidden_states
            .view([batch_size, height, width, channels])
            .permute([0, 3, 1, 2]);
        hidden_states = self.conv_block[2].forward(&hidden_states);
        hidden_states = Tensor::cat(&[residual, hidden_states], 1);
        hidden_states = self.conv_block[3].forward(&hidden_states);
        self.conv_block[4]
            .forward(&hidden_states)
            .squeeze_dim(2)
            .transpose(1, 2)
    }
}

#[derive(Debug)]
struct Head {
    encoder: Encoder,
    head: nn::Linear,
}

impl Head {
    fn new(path: &nn::Path<'_>, config: &Config) -> Self {
        Self {
            encoder: Encoder::new(&(path / "encoder"), config),
            head: nn::linear(
                path / "head",
                config.hidden_size,
                config.head_out_channels,
                Default::default(),
            ),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        self.head
            .forward(&self.encoder.forward(input))
            .softmax(2, Some(Kind::Float))
    }
}
