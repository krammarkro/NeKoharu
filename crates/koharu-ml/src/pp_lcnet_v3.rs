//! PP-LCNetV3 backbone used by PP-OCRv5 mobile recognition.
//!
//! Ported from Transformers at
//! https://github.com/huggingface/transformers/blob/5be474e9cc2c92acaa3ef14fe9e35ea9108b06c1/src/transformers/models/pp_lcnet_v3/modeling_pp_lcnet_v3.py

use koharu_torch::{
    Tensor,
    nn::{self, Module, ModuleT},
};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
pub(crate) enum Stride {
    Scalar(i64),
    Pair([i64; 2]),
}

impl Stride {
    fn pair(self) -> [i64; 2] {
        match self {
            Self::Scalar(value) => [value, value],
            Self::Pair(value) => value,
        }
    }

    fn uses_activation(self) -> bool {
        !matches!(self, Self::Scalar(2))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BlockConfig {
    #[serde(rename = "0")]
    pub kernel_size: i64,
    #[serde(rename = "1")]
    pub in_channels: i64,
    #[serde(rename = "2")]
    pub out_channels: i64,
    #[serde(rename = "3")]
    pub stride: Stride,
    #[serde(rename = "4")]
    pub use_squeeze_excitation: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub scale: f64,
    pub block_configs: Vec<Vec<BlockConfig>>,
    pub stem_channels: i64,
    pub stem_stride: i64,
    pub reduction: i64,
    pub divisor: i64,
    pub hidden_act: String,
    pub conv_symmetric_num: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scale: 1.0,
            block_configs: Vec::new(),
            stem_channels: 16,
            stem_stride: 2,
            reduction: 4,
            divisor: 8,
            hidden_act: "hardswish".into(),
            conv_symmetric_num: 4,
        }
    }
}

#[derive(Debug)]
struct ConvLayer {
    convolution: nn::Conv2D,
    normalization: nn::BatchNorm,
    activation: Option<String>,
}

impl ConvLayer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: &nn::Path<'_>,
        in_channels: i64,
        out_channels: i64,
        kernel_size: i64,
        stride: [i64; 2],
        groups: i64,
        activation: Option<&str>,
        bias: bool,
    ) -> Self {
        Self {
            convolution: nn::conv(
                path / "convolution",
                in_channels,
                out_channels,
                [kernel_size, kernel_size],
                nn::ConvConfigND {
                    stride,
                    padding: [kernel_size / 2, kernel_size / 2],
                    groups,
                    bias,
                    ..Default::default()
                },
            ),
            normalization: nn::batch_norm2d(
                path / "normalization",
                out_channels,
                Default::default(),
            ),
            activation: activation.map(str::to_owned),
        }
    }

    fn forward(&self, hidden_states: &Tensor) -> Tensor {
        activate(
            self.normalization
                .forward_t(&self.convolution.forward(hidden_states), false),
            self.activation.as_deref(),
        )
    }
}

fn activate(hidden_states: Tensor, activation: Option<&str>) -> Tensor {
    match activation {
        None => hidden_states,
        Some("silu") => hidden_states.silu(),
        Some("hardswish") => hidden_states.hardswish(),
        Some("relu") => hidden_states.relu(),
        Some(name) => panic!("unsupported PP-LCNetV3 activation {name}"),
    }
}

pub(crate) fn make_divisible(value: f64, divisor: i64) -> i64 {
    let value = value as i64;
    let mut rounded = ((value + divisor / 2) / divisor) * divisor;
    if (rounded as f64) < 0.9 * value as f64 {
        rounded += divisor;
    }
    rounded.max(divisor)
}

#[derive(Debug)]
struct LearnableAffine {
    scale: Tensor,
    bias: Tensor,
}

impl LearnableAffine {
    fn new(path: &nn::Path<'_>) -> Self {
        Self {
            scale: path.var("scale", &[1], nn::Init::Const(1.0)),
            bias: path.var("bias", &[1], nn::Init::Const(0.0)),
        }
    }

    fn forward(&self, hidden_states: Tensor) -> Tensor {
        &self.scale * hidden_states + &self.bias
    }
}

#[derive(Debug)]
struct ActivationWithAffine {
    activation: String,
    affine: LearnableAffine,
}

impl ActivationWithAffine {
    fn new(path: &nn::Path<'_>, activation: &str) -> Self {
        Self {
            activation: activation.into(),
            affine: LearnableAffine::new(&(path / "lab")),
        }
    }

    fn forward(&self, hidden_states: Tensor) -> Tensor {
        self.affine
            .forward(activate(hidden_states, Some(&self.activation)))
    }
}

#[derive(Debug)]
struct RepLayer {
    identity: Option<nn::BatchNorm>,
    conv_symmetric: Vec<ConvLayer>,
    conv_small_symmetric: Option<ConvLayer>,
    affine: LearnableAffine,
    activation: ActivationWithAffine,
    use_activation: bool,
}

impl RepLayer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: &nn::Path<'_>,
        in_channels: i64,
        out_channels: i64,
        kernel_size: i64,
        stride: Stride,
        groups: i64,
        num_conv_branches: usize,
        activation: &str,
    ) -> Self {
        let stride_pair = stride.pair();
        Self {
            identity: (out_channels == in_channels && stride_pair == [1, 1])
                .then(|| nn::batch_norm2d(path / "identity", in_channels, Default::default())),
            conv_symmetric: (0..num_conv_branches)
                .map(|index| {
                    ConvLayer::new(
                        &(path / "conv_symmetric" / index),
                        in_channels,
                        out_channels,
                        kernel_size,
                        stride_pair,
                        groups,
                        None,
                        false,
                    )
                })
                .collect(),
            conv_small_symmetric: (kernel_size > 1).then(|| {
                ConvLayer::new(
                    &(path / "conv_small_symmetric"),
                    in_channels,
                    out_channels,
                    1,
                    stride_pair,
                    groups,
                    None,
                    false,
                )
            }),
            affine: LearnableAffine::new(&(path / "lab")),
            activation: ActivationWithAffine::new(&(path / "act"), activation),
            use_activation: stride.uses_activation(),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let mut output = self
            .identity
            .as_ref()
            .map(|identity| identity.forward_t(input, false));
        if let Some(conv) = &self.conv_small_symmetric {
            let branch = conv.forward(input);
            output = Some(output.map_or(branch.shallow_clone(), |current| current + branch));
        }
        for conv in &self.conv_symmetric {
            let branch = conv.forward(input);
            output = Some(output.map_or(branch.shallow_clone(), |current| current + branch));
        }
        let output = self
            .affine
            .forward(output.expect("PP-LCNetV3 rep layer branches"));
        if self.use_activation {
            self.activation.forward(output)
        } else {
            output
        }
    }
}

#[derive(Debug)]
struct SqueezeExcitation {
    convolution_0: nn::Conv2D,
    convolution_2: nn::Conv2D,
}

impl SqueezeExcitation {
    fn new(path: &nn::Path<'_>, channels: i64, reduction: i64) -> Self {
        Self {
            convolution_0: nn::conv2d(
                path / "convolutions" / 0,
                channels,
                channels / reduction,
                1,
                Default::default(),
            ),
            convolution_2: nn::conv2d(
                path / "convolutions" / 2,
                channels / reduction,
                channels,
                1,
                Default::default(),
            ),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let weights = self
            .convolution_0
            .forward(&input.adaptive_avg_pool2d([1, 1]));
        input * self.convolution_2.forward(&weights.relu()).hardsigmoid()
    }
}

#[derive(Debug)]
struct DepthwiseSeparable {
    depthwise: RepLayer,
    squeeze_excitation: Option<SqueezeExcitation>,
    pointwise: RepLayer,
}

impl DepthwiseSeparable {
    fn new(path: &nn::Path<'_>, block: &BlockConfig, config: &Config) -> Self {
        let in_channels = make_divisible(block.in_channels as f64 * config.scale, config.divisor);
        let out_channels = make_divisible(block.out_channels as f64 * config.scale, config.divisor);
        Self {
            depthwise: RepLayer::new(
                &(path / "depthwise_convolution"),
                in_channels,
                in_channels,
                block.kernel_size,
                block.stride,
                in_channels,
                config.conv_symmetric_num,
                &config.hidden_act,
            ),
            squeeze_excitation: block.use_squeeze_excitation.then(|| {
                SqueezeExcitation::new(
                    &(path / "squeeze_excitation_module"),
                    in_channels,
                    config.reduction,
                )
            }),
            pointwise: RepLayer::new(
                &(path / "pointwise_convolution"),
                in_channels,
                out_channels,
                1,
                Stride::Scalar(1),
                1,
                config.conv_symmetric_num,
                &config.hidden_act,
            ),
        }
    }

    fn forward(&self, input: &Tensor) -> Tensor {
        let mut hidden_states = self.depthwise.forward(input);
        if let Some(squeeze_excitation) = &self.squeeze_excitation {
            hidden_states = squeeze_excitation.forward(&hidden_states);
        }
        self.pointwise.forward(&hidden_states)
    }
}

#[derive(Debug)]
pub(crate) struct Backbone {
    stem: ConvLayer,
    stages: Vec<Vec<DepthwiseSeparable>>,
}

impl Backbone {
    pub(crate) fn new(path: &nn::Path<'_>, config: &Config) -> Self {
        let stem_channels =
            make_divisible(config.stem_channels as f64 * config.scale, config.divisor);
        let stem = ConvLayer::new(
            &(path / "encoder" / "convolution"),
            3,
            stem_channels,
            3,
            [config.stem_stride, config.stem_stride],
            1,
            None,
            false,
        );
        let stages = config
            .block_configs
            .iter()
            .enumerate()
            .map(|(stage_index, blocks)| {
                blocks
                    .iter()
                    .enumerate()
                    .map(|(block_index, block)| {
                        DepthwiseSeparable::new(
                            &(path / "encoder" / "blocks" / stage_index / "layers" / block_index),
                            block,
                            config,
                        )
                    })
                    .collect()
            })
            .collect();
        Self { stem, stages }
    }

    pub(crate) fn forward(&self, pixel_values: &Tensor) -> Tensor {
        let mut hidden_states = self.stem.forward(pixel_values);
        for stage in &self.stages {
            for block in stage {
                hidden_states = block.forward(&hidden_states);
            }
        }
        hidden_states
    }
}
