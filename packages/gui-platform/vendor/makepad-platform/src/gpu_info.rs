 
pub enum GpuPerformance {
    Tier1, // quest 1
    Tier2, // quest 2
    Tier3, // intel, androids
    Tier4, // nvidia/ati/apple
    Tier5, // need a way to detect a 3090 :)
}

pub struct GpuInfo {
    pub min_uniform_vectors: u32,
    pub performance: GpuPerformance,
    pub vendor: String,
    pub renderer: String,
    /// Actual native renderer selected by the graphics API, once initialized.
    pub native_renderer: Option<GpuRenderer>,
} 

impl Default for GpuInfo{
    fn default()->Self{
        Self{// default to a nice gpu
            min_uniform_vectors:1024,
            performance: GpuPerformance::Tier4,
            vendor: "unknown".to_string(),
            renderer: "unknown".to_string(),
            native_renderer: None,
        }
    }
}

impl GpuInfo{

    pub fn init_from_info(&mut self, min_uniform_vectors:u32, vendor:String, renderer:String){
        self.vendor = vendor;
        self.vendor.make_ascii_lowercase();
        self.renderer = renderer;
        self.renderer.make_ascii_lowercase();

        self.min_uniform_vectors = min_uniform_vectors;

        // default tier 3
        self.performance = GpuPerformance::Tier3;
        
        // extremely useless performance separation. need to make this better
        if self.vendor.contains("qualcomm") && self.renderer.contains("540"){ // its a quest 1
            self.performance = GpuPerformance::Tier1;
        }
        if self.vendor.contains("qualcomm") && self.renderer.contains("610"){ // its a quest 2
            self.performance = GpuPerformance::Tier2;
        }
        if self.vendor.contains("intel"){
            self.performance = GpuPerformance::Tier3;
        }
        if self.vendor.contains("ati"){
            self.performance = GpuPerformance::Tier4;
        }
        if self.vendor.contains("nvidia"){
            self.performance = GpuPerformance::Tier4;
        }
    }

    pub fn is_low_on_uniform_vectors(&self)->bool{
        self.min_uniform_vectors < 512
    }
    
}

#[derive(Default)]
pub struct GpuRenderer {
    pub api: &'static str,
    pub renderer: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub shading_language_version: Option<String>,
    pub egl_vendor: Option<String>,
    pub warp: Option<bool>,
    pub software: bool,
}

impl GpuInfo {
    #[allow(dead_code)]
    pub(crate) fn record_native_renderer(&mut self, mut info: GpuRenderer) {
        if self.native_renderer.is_some() {
            return;
        }
        info.software |= is_software_renderer(&info.renderer);
        crate::log!(
            "GPU renderer: {} / {}; vendor={:?}, version={:?}, GLSL={:?}, EGL vendor={:?}, WARP={:?}",
            info.api, info.renderer, info.vendor, info.version, info.shading_language_version,
            info.egl_vendor, info.warp,
        );
        if info.software {
            crate::warning!("Software renderer in use ({} / {}): drawing runs on the CPU", info.api, info.renderer);
        }
        self.native_renderer = Some(info);
    }
}

fn is_software_renderer(renderer: &str) -> bool {
    [b"llvmpipe".as_slice(), b"softpipe", b"swrast", b"warp", b"microsoft basic render", b"swiftshader"]
        .iter()
        .any(|name| renderer.as_bytes().windows(name.len()).any(|part| part.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::is_software_renderer;

    #[test]
    fn distinguishes_software_renderers_from_hardware() {
        for name in ["llvmpipe (LLVM 19)", "SOFTPIPE", "Mesa swrast", "WARP",
                     "Microsoft Basic Render Driver", "ANGLE (Google SwiftShader)"] {
            assert!(is_software_renderer(name), "{name}");
        }
        for name in ["NV136", "NVIDIA GeForce GTX 1060", "Apple M4", "Adreno (TM) 740", ""] {
            assert!(!is_software_renderer(name), "{name}");
        }
    }
}

