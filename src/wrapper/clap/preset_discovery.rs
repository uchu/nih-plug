use clap_sys::factory::preset_discovery::{
    clap_preset_discovery_factory, clap_preset_discovery_indexer, clap_preset_discovery_location,
    clap_preset_discovery_metadata_receiver, clap_preset_discovery_provider,
    clap_preset_discovery_provider_descriptor, CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
};
use clap_sys::universal_plugin_id::clap_universal_plugin_id;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{c_char, c_void, CStr, CString};
use std::marker::PhantomData;
use std::ptr;
use std::sync::OnceLock;

use crate::prelude::{ClapPlugin, ClapPresetDiscovery};

/// Owns the CString data for a provider descriptor, keeping pointers stable.
struct ProviderDescriptor {
    _id: CString,
    _name: CString,
    _vendor: CString,
    descriptor: clap_preset_discovery_provider_descriptor,
}

impl ProviderDescriptor {
    fn new(id: &str, name: &str, vendor: &str) -> Box<Self> {
        let id_cstr = CString::new(id).unwrap();
        let name_cstr = CString::new(name).unwrap();
        let vendor_cstr = CString::new(vendor).unwrap();

        let descriptor = clap_preset_discovery_provider_descriptor {
            clap_version: CLAP_VERSION,
            id: id_cstr.as_ptr(),
            name: name_cstr.as_ptr(),
            vendor: vendor_cstr.as_ptr(),
        };

        let mut boxed = Box::new(Self {
            _id: id_cstr,
            _name: name_cstr,
            _vendor: vendor_cstr,
            descriptor,
        });

        // Fix up descriptor pointers to point into the heap-allocated CStrings
        boxed.descriptor.id = boxed._id.as_ptr();
        boxed.descriptor.name = boxed._name.as_ptr();
        boxed.descriptor.vendor = boxed._vendor.as_ptr();

        boxed
    }
}

/// A heap-allocated provider instance. Stored as raw pointer in `provider_data`.
struct ProviderInstance {
    discovery: Box<dyn ClapPresetDiscovery>,
    descriptor: Box<ProviderDescriptor>,
    _clap_id_cstr: CString,
    clap_id_cstr_ptr: *const c_char,
    indexer: *const clap_preset_discovery_indexer,
    provider: clap_preset_discovery_provider,
}

impl ProviderInstance {
    fn new(
        discovery: Box<dyn ClapPresetDiscovery>,
        clap_id: &str,
        indexer: *const clap_preset_discovery_indexer,
    ) -> *const clap_preset_discovery_provider {
        let descriptor = ProviderDescriptor::new(
            discovery.provider_id(),
            discovery.provider_name(),
            discovery.provider_vendor(),
        );

        let clap_id_cstr = CString::new(clap_id).unwrap();
        let clap_id_cstr_ptr = clap_id_cstr.as_ptr();

        let mut instance = Box::new(Self {
            discovery,
            descriptor,
            _clap_id_cstr: clap_id_cstr,
            clap_id_cstr_ptr,
            indexer,
            provider: clap_preset_discovery_provider {
                desc: ptr::null(),
                provider_data: ptr::null_mut(),
                init: Some(provider_init),
                destroy: Some(provider_destroy),
                get_metadata: Some(provider_get_metadata),
                get_extension: Some(provider_get_extension),
            },
        });

        // Fix up self-referential pointers
        instance.provider.desc = &instance.descriptor.descriptor;
        instance.clap_id_cstr_ptr = instance._clap_id_cstr.as_ptr();

        let raw = Box::into_raw(instance);
        unsafe {
            (*raw).provider.provider_data = raw as *mut c_void;
        }

        // Return pointer to the embedded provider struct
        unsafe { &raw const (*raw).provider }
    }
}

unsafe extern "C" fn provider_init(
    provider: *const clap_preset_discovery_provider,
) -> bool {
    if provider.is_null() {
        return false;
    }
    let instance = &*((*provider).provider_data as *const ProviderInstance);
    let indexer = &*instance.indexer;

    // Declare a single plugin-internal location for all presets
    let location_name = CString::new(instance.discovery.provider_name()).unwrap();
    let location = clap_preset_discovery_location {
        flags: clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT
            | clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_IS_USER_CONTENT,
        name: location_name.as_ptr(),
        kind: CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN,
        location: ptr::null(),
    };

    if let Some(declare_location) = indexer.declare_location {
        declare_location(instance.indexer, &location);
    }

    true
}

unsafe extern "C" fn provider_destroy(
    provider: *const clap_preset_discovery_provider,
) {
    if !provider.is_null() {
        let instance_ptr = (*provider).provider_data as *mut ProviderInstance;
        if !instance_ptr.is_null() {
            drop(Box::from_raw(instance_ptr));
        }
    }
}

unsafe extern "C" fn provider_get_metadata(
    provider: *const clap_preset_discovery_provider,
    _location_kind: u32,
    _location: *const c_char,
    metadata_receiver: *const clap_preset_discovery_metadata_receiver,
) -> bool {
    if provider.is_null() || metadata_receiver.is_null() {
        return false;
    }
    let instance = &*((*provider).provider_data as *const ProviderInstance);
    let receiver = &*metadata_receiver;

    let entries = instance.discovery.enumerate_presets();
    let abi = c"clap";
    let plugin_id = clap_universal_plugin_id {
        abi: abi.as_ptr(),
        id: instance.clap_id_cstr_ptr,
    };

    for entry in &entries {
        let name = CString::new(entry.name.as_str()).unwrap_or_default();
        let load_key = CString::new(entry.load_key.as_str()).unwrap_or_default();

        if let Some(begin_preset) = receiver.begin_preset {
            if !begin_preset(metadata_receiver, name.as_ptr(), load_key.as_ptr()) {
                continue;
            }
        }

        if let Some(add_plugin_id) = receiver.add_plugin_id {
            add_plugin_id(metadata_receiver, &plugin_id);
        }

        if let Some(set_flags) = receiver.set_flags {
            set_flags(metadata_receiver, entry.flags);
        }

        if let Some(ref creator) = entry.creator {
            if let Ok(creator_cstr) = CString::new(creator.as_str()) {
                if let Some(add_creator) = receiver.add_creator {
                    add_creator(metadata_receiver, creator_cstr.as_ptr());
                }
            }
        }

        if let Some(ref description) = entry.description {
            if let Ok(desc_cstr) = CString::new(description.as_str()) {
                if let Some(set_description) = receiver.set_description {
                    set_description(metadata_receiver, desc_cstr.as_ptr());
                }
            }
        }
    }

    true
}

unsafe extern "C" fn provider_get_extension(
    _provider: *const clap_preset_discovery_provider,
    _extension_id: *const c_char,
) -> *const c_void {
    ptr::null()
}

/// The preset discovery factory. One static instance per plugin type in the macro.
pub struct PresetDiscoveryFactory<P: ClapPlugin> {
    _phantom: PhantomData<P>,
}

impl<P: ClapPlugin> PresetDiscoveryFactory<P> {
    pub fn factory() -> &'static clap_preset_discovery_factory {
        static FACTORY: OnceLock<clap_preset_discovery_factory> = OnceLock::new();
        FACTORY.get_or_init(|| clap_preset_discovery_factory {
            count: Some(factory_count::<P>),
            get_descriptor: Some(factory_get_descriptor::<P>),
            create: Some(factory_create::<P>),
        })
    }
}

static DESCRIPTOR_STORE: OnceLock<Box<ProviderDescriptor>> = OnceLock::new();

unsafe extern "C" fn factory_count<P: ClapPlugin>(
    _factory: *const clap_preset_discovery_factory,
) -> u32 {
    if P::clap_preset_discovery().is_some() {
        1
    } else {
        0
    }
}

unsafe extern "C" fn factory_get_descriptor<P: ClapPlugin>(
    _factory: *const clap_preset_discovery_factory,
    index: u32,
) -> *const clap_preset_discovery_provider_descriptor {
    if index != 0 {
        return ptr::null();
    }

    let descriptor = DESCRIPTOR_STORE.get_or_init(|| {
        if let Some(discovery) = P::clap_preset_discovery() {
            ProviderDescriptor::new(
                discovery.provider_id(),
                discovery.provider_name(),
                discovery.provider_vendor(),
            )
        } else {
            ProviderDescriptor::new("", "", "")
        }
    });

    &descriptor.descriptor
}

unsafe extern "C" fn factory_create<P: ClapPlugin>(
    _factory: *const clap_preset_discovery_factory,
    indexer: *const clap_preset_discovery_indexer,
    _provider_id: *const c_char,
) -> *const clap_preset_discovery_provider {
    if indexer.is_null() {
        return ptr::null();
    }

    match P::clap_preset_discovery() {
        Some(discovery) => ProviderInstance::new(discovery, P::CLAP_ID, indexer),
        None => ptr::null(),
    }
}
