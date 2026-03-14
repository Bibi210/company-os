use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::constants;
use crate::error::ConfigError;
use crate::types::*;

pub struct CompanyConfig {
    pub flow_control: FlowControl,
    pub review_protocol: ReviewProtocol,
    pub human_review_triggers: HumanReviewTriggers,
    pub personas: HashMap<PersonaId, ArtifactEnvelope<PersonaSpec>>,
    pub root_dir: PathBuf,
}

impl CompanyConfig {
    /// Load all config from the company-os root directory.
    pub fn load(root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let root = root.as_ref().to_path_buf();

        let flow_control = Self::load_flow_control(&root)?;
        let review_protocol = Self::load_review_protocol(&root)?;
        let human_review_triggers = Self::load_human_review_triggers(&root)?;
        let personas = Self::load_personas(&root)?;

        Ok(Self {
            flow_control,
            review_protocol,
            human_review_triggers,
            personas,
            root_dir: root,
        })
    }

    /// Resolve schema path for a given artifact kind.
    pub fn schema_path(&self, kind: ArtifactKind) -> PathBuf {
        self.root_dir
            .join(constants::SCHEMAS_DIR)
            .join(kind.schema_filename())
    }

    fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
        if !path.exists() {
            return Err(ConfigError::NotFound {
                path: path.to_path_buf(),
            });
        }
        let content = std::fs::read_to_string(path)?;
        serde_yaml::from_str(&content).map_err(|e| ConfigError::YamlParse {
            path: path.to_path_buf(),
            source: e,
        })
    }

    fn load_flow_control(root: &Path) -> Result<FlowControl, ConfigError> {
        let path = root.join(constants::CONFIG_FLOW_CONTROL);
        let envelope: ArtifactEnvelope<FlowControlSpec> = Self::load_yaml(&path)?;
        Ok(envelope.spec.spec)
    }

    fn load_review_protocol(root: &Path) -> Result<ReviewProtocol, ConfigError> {
        let path = root.join(constants::CONFIG_REVIEW_PROTOCOL);
        let envelope: ArtifactEnvelope<ReviewProtocolSpec> = Self::load_yaml(&path)?;
        Ok(envelope.spec.spec)
    }

    fn load_human_review_triggers(root: &Path) -> Result<HumanReviewTriggers, ConfigError> {
        let path = root.join(constants::CONFIG_HUMAN_REVIEW_TRIGGERS);
        let envelope: ArtifactEnvelope<HumanReviewTriggersSpec> = Self::load_yaml(&path)?;
        Ok(envelope.spec.spec)
    }

    fn load_personas(
        root: &Path,
    ) -> Result<HashMap<PersonaId, ArtifactEnvelope<PersonaSpec>>, ConfigError> {
        let mut personas = HashMap::new();
        let dir = root.join(constants::PERSONAS_DIR);

        for persona_id in PersonaId::all() {
            let path = dir.join(format!("{}.{}", persona_id.as_str(), constants::EXT_YML));
            if path.exists() {
                let envelope: ArtifactEnvelope<PersonaSpec> = Self::load_yaml(&path)?;
                personas.insert(*persona_id, envelope);
            }
        }

        Ok(personas)
    }
}
