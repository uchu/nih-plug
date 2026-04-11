use super::Plugin;
use crate::params::internals::ParamPtr;
use crate::prelude::{ClapFeature, RemoteControlsContext};

/// Provides auxiliary metadata needed for a CLAP plugin.
#[allow(unused_variables)]
pub trait ClapPlugin: Plugin {
    /// A unique ID that identifies this particular plugin. This is usually in reverse domain name
    /// notation, e.g. `com.manufacturer.plugin-name`.
    const CLAP_ID: &'static str;
    /// An optional short description for the plugin.
    const CLAP_DESCRIPTION: Option<&'static str>;
    /// The URL to the plugin's manual, if available.
    const CLAP_MANUAL_URL: Option<&'static str>;
    /// The URL to the plugin's support page, if available.
    const CLAP_SUPPORT_URL: Option<&'static str>;
    /// Keywords describing the plugin. The host may use this to classify the plugin in its plugin
    /// browser.
    const CLAP_FEATURES: &'static [ClapFeature];

    /// If set, this informs the host about the plugin's capabilities for polyphonic modulation.
    const CLAP_POLY_MODULATION_CONFIG: Option<PolyModulationConfig> = None;

    /// This function can be implemented to define plugin-specific [remote control
    /// pages](https://github.com/free-audio/clap/blob/main/include/clap/ext/draft/remote-controls.h)
    /// that the host can use to provide better hardware mapping for a plugin. See the linked
    /// extension for more information.
    fn remote_controls(&self, context: &mut impl RemoteControlsContext) {}

    /// Return a preset discovery provider if this plugin supports CLAP preset discovery.
    /// This is called at factory level (no plugin instance) to enumerate presets for the host.
    fn clap_preset_discovery() -> Option<Box<dyn ClapPresetDiscovery>> {
        None
    }

    /// Load a preset identified by the given location triple. Called by the host when the user
    /// selects a preset from the host's browser. The `context` can be used to set parameter values.
    fn load_preset_from_location(
        &mut self,
        location_kind: u32,
        location: &str,
        load_key: &str,
        context: &dyn PresetLoadContext,
    ) -> bool {
        false
    }
}

/// A context passed to [`ClapPlugin::load_preset_from_location`] that allows setting parameter
/// values from the wrapper level. Uses `ParamPtr` and normalized values to remain object-safe.
pub trait PresetLoadContext {
    /// Set a parameter's normalized value directly via its pointer.
    /// # Safety
    /// The caller must ensure the `ParamPtr` is valid (obtained from `Param::as_ptr()`).
    fn set_param_normalized(&self, ptr: ParamPtr, normalized: f32);
}

/// Trait for providing preset discovery to CLAP hosts. Implement this to enumerate your plugin's
/// presets at scan time (no plugin instance required).
pub trait ClapPresetDiscovery: Send + Sync {
    fn provider_id(&self) -> &str;
    fn provider_name(&self) -> &str;
    fn provider_vendor(&self) -> &str;
    fn enumerate_presets(&self) -> Vec<ClapPresetEntry>;
}

/// A single preset entry for CLAP preset discovery.
pub struct ClapPresetEntry {
    pub name: String,
    pub load_key: String,
    pub creator: Option<String>,
    pub description: Option<String>,
    pub flags: u32,
}

/// Configuration for the plugin's polyphonic modulation options, if it supports .
pub struct PolyModulationConfig {
    /// The maximum number of voices this plugin will ever use. Call the context's
    /// `set_current_voice_capacity()` method during initialization or audio processing to set the
    /// polyphony limit.
    pub max_voice_capacity: u32,
    /// If set to `true`, then the host may send note events for the same channel and key, but using
    /// different voice IDs. Bitwig Studio, for instance, can use this to do voice stacking. After
    /// enabling this, you should always prioritize using voice IDs to map note events to voices.
    pub supports_overlapping_voices: bool,
}
