//! Dedicated PP-OCRv5 East-Slavic recognizer for Russian, Belarusian, and Ukrainian text.

mod config;
mod model;

use anyhow::{Context, Result};
use image::DynamicImage;
use koharu_torch::Device;

use crate::{
    backend::TryIntoDevice,
    pp_ocr_v6::rec::{PPOCRV6MediumRecImageProcessor, TextRecognition},
};

use self::{config::Config, model::Model};

crate::model_repository!(
    "PaddlePaddle/eslav_PP-OCRv5_mobile_rec_safetensors" @ "feb3757f69257ed5320044604217032a7b90b789" {
        CONFIG = "config.json",
        WEIGHTS = "model.safetensors",
        PROCESSOR = "preprocessor_config.json",
    }
);

#[derive(Debug)]
pub struct PPOCRv5Cyrillic {
    device: Device,
    model: Model,
    processor: PPOCRV6MediumRecImageProcessor,
}

impl PPOCRv5Cyrillic {
    pub async fn load(device: crate::Device) -> Result<Self> {
        let device: Device = device.try_into_device()?;
        let config_path = CONFIG
            .resolve()
            .await
            .context("failed to resolve PP-OCRv5 East-Slavic recognition config")?;
        let weights_path = WEIGHTS
            .resolve()
            .await
            .context("failed to resolve PP-OCRv5 East-Slavic recognition weights")?;
        let processor_path = PROCESSOR
            .resolve()
            .await
            .context("failed to resolve PP-OCRv5 East-Slavic recognition processor")?;

        let config = Config::from_file(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut processor = PPOCRV6MediumRecImageProcessor::from_file(&processor_path)
            .with_context(|| format!("failed to read {}", processor_path.display()))?;
        // PP-OCRv5's RecResizeImg uses (pixel / 255 - 0.5) / 0.5, unlike the shared processor defaults.
        // https://github.com/PaddlePaddle/PaddleOCR/blob/db4b14b6bde5d4cbd3cd3e62906e577dad358c21/ppocr/data/imaug/rec_img_aug.py
        processor.image_mean = vec![0.5; 3];
        processor.image_std = vec![0.5; 3];
        if processor.character_list.len() != config.head_out_channels as usize {
            anyhow::bail!(
                "PP-OCRv5 recognition vocabulary has {} entries but the head has {} outputs",
                processor.character_list.len(),
                config.head_out_channels
            );
        }
        let mut model = Model::new(&config, device);
        model
            .load(&weights_path)
            .with_context(|| format!("failed to load {}", weights_path.display()))?;

        Ok(Self {
            device,
            model,
            processor,
        })
    }

    pub fn inference(&self, image: &DynamicImage) -> Result<TextRecognition> {
        koharu_torch::no_grad(|| {
            let pixel_values = self.processor.preprocess(image, self.device)?;
            let probabilities = self.model.forward(&pixel_values);
            self.processor.postprocess(&probabilities)
        })
    }
}

#[cfg(test)]
mod tests {
    use koharu_runtime::{Feature, Runtime};

    use super::*;

    #[tokio::test]
    #[ignore = "downloads the checkpoint and LibTorch runtime"]
    async fn loads_east_slavic_recognizer_checkpoint() {
        Runtime::discover([Feature::Torch])
            .unwrap()
            .initialize()
            .await
            .unwrap();
        let model = PPOCRv5Cyrillic::load(crate::Device::cpu()).await.unwrap();
        model.inference(&DynamicImage::new_rgb8(128, 32)).unwrap();
    }
}
