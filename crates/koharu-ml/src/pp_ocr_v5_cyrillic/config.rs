use std::{fs, path::Path};

use anyhow::Result;
use serde::Deserialize;

use crate::pp_lcnet_v3::Config as PPLCNetV3Config;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub hidden_act: String,
    pub backbone_config: PPLCNetV3Config,
    pub hidden_size: i64,
    pub mlp_ratio: f64,
    pub depth: usize,
    pub head_out_channels: i64,
    pub conv_kernel_size: Vec<i64>,
    pub qkv_bias: bool,
    pub num_attention_heads: i64,
    pub layer_norm_eps: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hidden_act: "silu".into(),
            backbone_config: PPLCNetV3Config::default(),
            hidden_size: 120,
            mlp_ratio: 2.0,
            depth: 2,
            head_out_channels: 519,
            conv_kernel_size: vec![1, 3],
            qkv_bias: true,
            num_attention_heads: 8,
            layer_norm_eps: 1e-6,
        }
    }
}

impl Config {
    pub(crate) fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }
}
