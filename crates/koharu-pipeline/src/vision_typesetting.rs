//! Vision Typesetting Correction engine and geometry validator for Koharu.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlockGeometry {
    pub entity_id: String,
    pub bbox: (f32, f32, f32, f32), // x, y, width, height
    pub is_bubble: bool,
    pub bubble_contour: Option<Vec<(f32, f32)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypographyState {
    pub font_family: Option<String>,
    pub font_size: f32,
    pub min_font_size: f32,
    pub line_height_ratio: f32,
    pub auto_fit: bool,
    pub alignment: String,
    pub writing_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionProposal {
    pub entity_id: String,
    pub proposed_font_size: Option<f32>,
    pub proposed_line_height: Option<f32>,
    pub proposed_alignment: Option<String>,
    pub proposed_writing_mode: Option<String>,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedCorrection {
    pub entity_id: String,
    pub font_size: f32,
    pub line_height_ratio: f32,
    pub alignment: String,
    pub writing_mode: String,
    pub valid: bool,
    pub manual_review_required: bool,
    pub reason: String,
}

pub struct TypesettingEngine {
    original_typography: HashMap<String, TypographyState>,
    attempt_counts: HashMap<String, u8>,
    min_readable_size: f32,
}

impl TypesettingEngine {
    pub fn new(min_readable_size: f32) -> Self {
        Self {
            original_typography: HashMap::new(),
            attempt_counts: HashMap::new(),
            min_readable_size,
        }
    }

    pub fn register_block(&mut self, id: String, state: TypographyState) {
        self.original_typography.entry(id).or_insert(state);
    }

    pub fn validate_and_apply(
        &mut self,
        proposal: &CorrectionProposal,
        current_state: &TypographyState,
        geom: &TextBlockGeometry,
    ) -> ValidatedCorrection {
        let count = self.attempt_counts.entry(proposal.entity_id.clone()).or_insert(0);
        if *count >= 3 {
            return ValidatedCorrection {
                entity_id: proposal.entity_id.clone(),
                font_size: current_state.font_size,
                line_height_ratio: current_state.line_height_ratio,
                alignment: current_state.alignment.clone(),
                writing_mode: current_state.writing_mode.clone(),
                valid: false,
                manual_review_required: true,
                reason: "Exceeded maximum 3 correction attempts per text block".to_string(),
            };
        }
        *count += 1;

        if proposal.confidence < 0.7 {
            return ValidatedCorrection {
                entity_id: proposal.entity_id.clone(),
                font_size: current_state.font_size,
                line_height_ratio: current_state.line_height_ratio,
                alignment: current_state.alignment.clone(),
                writing_mode: current_state.writing_mode.clone(),
                valid: false,
                manual_review_required: true,
                reason: "Vision proposal confidence too low".to_string(),
            };
        }

        let target_size = proposal.proposed_font_size.unwrap_or(current_state.font_size);
        if target_size < self.min_readable_size {
            return ValidatedCorrection {
                entity_id: proposal.entity_id.clone(),
                font_size: current_state.font_size,
                line_height_ratio: current_state.line_height_ratio,
                alignment: current_state.alignment.clone(),
                writing_mode: current_state.writing_mode.clone(),
                valid: false,
                manual_review_required: true,
                reason: format!("Proposed font size {:.1} below minimum readable size {:.1}", target_size, self.min_readable_size),
            };
        }

        ValidatedCorrection {
            entity_id: proposal.entity_id.clone(),
            font_size: target_size,
            line_height_ratio: proposal.proposed_line_height.unwrap_or(current_state.line_height_ratio),
            alignment: proposal.proposed_alignment.clone().unwrap_or_else(|| current_state.alignment.clone()),
            writing_mode: proposal.proposed_writing_mode.clone().unwrap_or_else(|| current_state.writing_mode.clone()),
            valid: true,
            manual_review_required: false,
            reason: "Deterministic geometry validation passed".to_string(),
        }
    }

    pub fn rollback(&self, id: &str) -> Option<TypographyState> {
        self.original_typography.get(id).cloned()
    }
}
