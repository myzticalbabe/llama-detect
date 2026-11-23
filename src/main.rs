use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize, Debug, Default)]
struct SystemInfo {
    total_ram_gb: f64,
    available_ram_gb: f64,
    total_ram_bytes: u64,
    available_ram_bytes: u64,
    logical_cores: u32,
    physical_cores: u32,
}

#[derive(Serialize, Debug, Default)]
struct GpuInfo {
    name: String,
    vram_bytes: u64,
    vram_gb: f64,
    driver_version: String,
    vulkan_available: bool,
    level_zero_available: bool,
}

#[derive(Serialize, Debug, Default)]
struct NpuInfo {
    name: String,
    available: bool,
    driver_version: String,
}

#[derive(Serialize, Debug, Default)]
struct BackendInfo {
    name: String,
    available: bool,
    devices: Vec<String>,
}

#[derive(Serialize, Debug, Default)]
struct ModelInfo {
    path: String,
    size_bytes: u64,
    size_gb: f64,
    display_name: String,
}

#[derive(Serialize, Debug, Default)]
struct Recommendation {
    device: String,
    n_gpu_layers: String,
    split_mode: String,
    main_gpu: u32,
    tensor_split: String,
    args: Vec<String>,
    summary: String,
    target_memory_gb: f64,
    available_vram_gb: f64,
    model_size_gb: f64,
    kv_cache_gb: f64,
    memory_adequate: bool,
    output_tokens_per_second_estimate: f64,
}

#[derive(Serialize, Debug)]
struct HardwareConfig {
    system: SystemInfo,
    gpu: Vec<GpuInfo>,
    npu: Option<NpuInfo>,
    backends: Vec<BackendInfo>,
    model: Option<ModelInfo>,
    recommendation: Recommendation,
}

fn run_powershell(script: &str) -> String {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

fn probe_system_info() -> SystemInfo {
    let total = run_powershell(
        "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
    )
    .parse::<u64>()
    .unwrap_or(16_000_000_000);

    let free_kb = run_powershell(
        "(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory",
    )
    .parse::<u64>()
    .unwrap_or(8_000_000);
    let available = free_kb * 1024;

    let logical = run_powershell("(Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors")
        .parse::<u32>()
        .unwrap_or(22);
    let physical = run_powershell(
        "Get-CimInstance Win32_ComputerSystem | Select-Object -ExpandProperty NumberOfProcessors",
    );

    let cores = run_powershell(
        "(Get-CimInstance Win32_Processor).NumberOfCores",
    )
    .parse::<u32>()
    .unwrap_or(16);

    SystemInfo {
        total_ram_bytes: total,
        total_ram_gb: (total as f64) / 1_000_000_000.0,
        available_ram_bytes: available,
        available_ram_gb: (available as f64) / 1_000_000_000.0,
        logical_cores: logical,
        physical_cores: cores,
    }
}

fn probe_gpu() -> Vec<GpuInfo> {
    let mut gpus = Vec::new();

    let script = r#"
$gpu = Get-CimInstance Win32_VideoController | Select-Object -First 1
$vram = $gpu.AdapterRAM
if (-not $vram -or $vram -eq 0) { $vram = 2147483648 }
[PSCustomObject]@{
    Name = $gpu.Name
    RAM = $vram
    Driver = $gpu.DriverVersion
} | ConvertTo-Json -Compress
"#;
    let raw = run_powershell(script);

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let name = extract_json_string(line, "Name").unwrap_or_default();
        let wmi_ram = extract_json_u64(line, "RAM").unwrap_or(0);
        let driver = extract_json_string(line, "Driver").unwrap_or_default();

        let (vk_total, _vk_free) = probe_vulkan_device_memory();
        let ram = if vk_total > 0 { vk_total } else { wmi_ram };

        let vulkan = run_powershell("vulkaninfo 2>$null | Select-String 'Vulkan Instance'")
            .contains("Vulkan");

        let level_zero = PathBuf::from(
            r"C:\Program Files (x86)\Intel\oneAPI\compiler\2026.0\bin\sycl-ls.exe",
        )
        .exists();

        gpus.push(GpuInfo {
            name: name.clone(),
            vram_bytes: ram,
            vram_gb: (ram as f64) / 1_000_000_000.0,
            driver_version: driver,
            vulkan_available: vulkan,
            level_zero_available: level_zero,
        });
    }

    if gpus.is_empty() {
        gpus.push(GpuInfo {
            name: "Intel(R) Arc(TM) 140T GPU (8GB)".into(),
            vram_bytes: 8_000_000_000,
            vram_gb: 8.0,
            driver_version: String::new(),
            vulkan_available: true,
            level_zero_available: true,
        });
    }

    gpus
}

fn probe_npu() -> Option<NpuInfo> {
    let script = r#"
Get-CimInstance Win32_PNPEntity | Where-Object { $_.Name -like '*AI Boost*' -or $_.Name -like '*NPU*' -or $_.Name -like '*Intel*Neural*' } | ForEach-Object {
    [PSCustomObject]@{
        Name = $_.Name
        DeviceID = $_.DeviceID
        Status = $_.Status
    } | ConvertTo-Json -Compress
}"#;
    let raw = run_powershell(script);
    if raw.is_empty() {
        return None;
    }
    let name = extract_json_string(&raw, "Name").unwrap_or("Intel AI Boost".into());
    Some(NpuInfo {
        name,
        available: true,
        driver_version: run_powershell(
            "Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceName -like '*AI Boost*' } | Select-Object -ExpandProperty DriverVersion",
        ),
    })
}

fn probe_vulkan_device_memory() -> (u64, u64) {
    let total_script = r#"
$v = vulkaninfo 2>$null | Select-String 'VkDeviceSize'
if (-not $v) { $v = vulkaninfo 2>$null | Select-String 'DeviceLocalMemory' }
if ($v) {
    $line = $v | Select-Object -Last 1
    $t = $line.ToString()
    if ($t -match '(\d+)\s*MiB') { [uint64]($Matches[1]) * 1024 * 1024 }
    elseif ($t -match '(\d+)\s*bytes') { [uint64]($Matches[1]) }
    else { 0 }
} else { 0 }
"#;
    let total = run_powershell(total_script).trim().parse::<u64>().unwrap_or(0);

    let free_script = r#"
$v = vulkaninfo 2>$null | Select-String 'DeviceLocalMemory'
if ($v) {
    $parts = $v.ToString() -split ':\s*'
    if ($parts.Count -ge 2) {
        $val = $parts[1].Trim()
        if ($val -match '(\d+)\s*MiB') { [uint64]($Matches[1]) * 1024 * 1024 }
        elseif ($val -match '(\d+)\s*bytes') { [uint64]($Matches[1]) }
        else { 0 }
    } else { 0 }
} else { 0 }
"#;
    let free = run_powershell(free_script).trim().parse::<u64>().unwrap_or(0);

    (if total > 0 { total } else { 8_000_000_000 }, if free > 0 { free } else { total })
}

fn probe_backends(server_path: &Option<String>) -> Vec<BackendInfo> {
    let mut backends = Vec::new();

    let cpu = BackendInfo {
        name: "cpu".into(),
        available: true,
        devices: vec![format!(
            "CPU ({})",
            run_powershell("(Get-CimInstance Win32_Processor).Name")
        )],
    };
    backends.push(cpu);

    let vulkan = if run_powershell("vulkaninfo 2>$null | Select-String 'GPU'")
        .contains("GPU")
    {
        BackendInfo {
            name: "vulkan".into(),
            available: true,
            devices: vec!["Intel(R) Arc(TM) 140T GPU (8GB)".into()],
        }
    } else {
        BackendInfo {
            name: "vulkan".into(),
            available: false,
            devices: vec![],
        }
    };
    backends.push(vulkan);

    let sycl_script = r#"
& "C:\Program Files (x86)\Intel\oneAPI\compiler\2026.0\bin\sycl-ls.exe" 2>$null | Select-String -Pattern 'gpu|gpu' -SimpleMatch
"#;
    let sycl_out = run_powershell(sycl_script);
    backends.push(BackendInfo {
        name: "sycl".into(),
        available: !sycl_out.is_empty(),
        devices: if sycl_out.is_empty() {
            vec![]
        } else {
            sycl_out.lines().map(|l| l.trim().to_string()).collect()
        },
    });

    let openvino_found = if let Some(ref path) = server_path {
        let dir = PathBuf::from(path).parent().map(|p| p.to_path_buf());
        dir.map(|d| d.join("ggml-openvino.dll").exists()).unwrap_or(false)
    } else {
        false
    };
    backends.push(BackendInfo {
        name: "openvino".into(),
        available: openvino_found,
        devices: vec![],
    });

    if let Some(path) = server_path {
        let pb = PathBuf::from(path);
        if pb.exists() {
            let list_out = Command::new(path).arg("--list-devices").output();
            if let Ok(o) = list_out {
                let out_str = String::from_utf8_lossy(&o.stdout);
                for line in out_str.lines() {
                    if line.contains("Available devices:") {
                        continue;
                    }
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        backends.push(BackendInfo {
                            name: "llama-native".into(),
                            available: true,
                            devices: vec![trimmed.to_string()],
                        });
                    }
                }
            }
        }
    }

    backends
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    if let Some(start) = json.find(&pattern) {
        let val_start = start + pattern.len();
        if let Some(end) = json[val_start..].find('\"') {
            return Some(json[val_start..val_start + end].to_string());
        }
    }
    let pattern2 = format!("\"{}\": \"", key);
    if let Some(start) = json.find(&pattern2) {
        let val_start = start + pattern2.len();
        if let Some(end) = json[val_start..].find('\"') {
            return Some(json[val_start..val_start + end].to_string());
        }
    }
    None
}

fn extract_json_u64(json: &str, key: &str) -> Option<u64> {
    let pattern = format!("\"{}\":", key);
    if let Some(start) = json.find(&pattern) {
        let val_start = start + pattern.len();
        let rest = &json[val_start..];
        let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        return num_str.parse::<u64>().ok();
    }
    None
}

fn read_model_info(state_path: &str) -> Option<ModelInfo> {
    let pb = PathBuf::from(state_path);
    if !pb.exists() {
        return None;
    }
    let content = std::fs::read_to_string(state_path).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('|').collect();
        if fields.len() >= 3 {
            let model_path = fields[0].to_string();
            let ctx_str = fields[1].to_string();
            let fname = fields[2].to_string();
            let display = fields.get(4).unwrap_or(&fields[2]).to_string();
            let mpb = PathBuf::from(&model_path);
            let size = std::fs::metadata(&mpb).ok().map(|m| m.len()).unwrap_or(0);
            return Some(ModelInfo {
                path: model_path,
                size_bytes: size,
                size_gb: (size as f64) / 1_000_000_000.0,
                display_name: display,
            });
        }
    }
    None
}

fn get_vulkan_free_mib(server_path: &Option<String>) -> f64 {
    if let Some(path) = server_path {
        let pb = PathBuf::from(path);
        if pb.exists() {
            if let Ok(o) = Command::new(path).arg("--list-devices").output() {
                let out = String::from_utf8_lossy(&o.stdout);
                for line in out.lines() {
                    if let Some(end) = line.find(" free)") {
                        let before = &line[..end];
                        if let Some(start) = before.rfind('(') {
                            let num_str = &before[start+1..];
                            let num_str: String = num_str.chars().take_while(|c| c.is_digit(10) || *c == '.').collect();
                            if let Ok(val) = num_str.parse::<f64>() {
                                return val;
                            }
                        }
                    }
                }
            }
        }
    }
    0.0
}

fn compute_recommendation(
    sys: &SystemInfo,
    gpus: &[GpuInfo],
    npu: &Option<NpuInfo>,
    backends: &[BackendInfo],
    model: &Option<ModelInfo>,
) -> Recommendation {
    let ctx_size: u64 = 131_072;
    let n_layers: u32 = 62;
    let _server_path = backends.iter().find(|b| b.name == "llama-native");

    let model_size = model.as_ref().map(|m| m.size_gb).unwrap_or(5.0);
    let model_bytes = (model_size * 1_000_000_000.0) as u64;

    let kv_cache_bytes = ctx_size as u64 * 2 * n_layers as u64;
    let kv_cache_gb = (kv_cache_bytes as f64) / 1_000_000_000.0;
    let total_needed_gb = model_size + kv_cache_gb + 1.0;

    let gpu_vram = gpus.first().map(|g| g.vram_gb).unwrap_or(0.0);

    let mut args = vec!["--mlock".to_string()];
    let mut device = "cpu".to_string();
    let mut ngl = "0".to_string();
    let mut split_mode = "none".to_string();
    let mut main_gpu = 0u32;
    let mut tensor_split = String::new();
    let mut summary = String::new();
    let mut target_mem = 0.0f64;
    let mut tps_est = 10.0f64;

    let has_vulkan = backends.iter().any(|b| b.name == "vulkan" && b.available);
    let has_sycl = backends.iter().any(|b| b.name == "sycl" && b.available);
    let has_openvino = backends.iter().any(|b| b.name == "openvino" && b.available);
    let has_npu = npu.as_ref().map(|n| n.available).unwrap_or(false);

    let ops = |available_gb: f64, _model_gb: f64| -> bool { available_gb * 0.85 >= model_size };

    if has_vulkan && gpu_vram >= 5.0 {
        if ops(gpu_vram, model_size) {
            device = "gpu".to_string();
            ngl = "all".to_string();
            split_mode = "none".to_string();
            target_mem = model_size;
            tps_est = 40.0;
            args.insert(0, "-ngl".to_string());
            args.insert(1, "all".to_string());
            let gpu_name = gpus.first().map(|g| g.name.as_str()).unwrap_or("GPU");
            summary = format!(
                "All layers offloaded to {} ({:.1} GB VRAM) — model fits entirely in GPU",
                gpu_name, gpu_vram
            );
        } else {
            device = "gpu".to_string();
            ngl = "all".to_string();
            split_mode = "row".to_string();
            target_mem = gpu_vram * 0.85;
            tps_est = 35.0;
            args.insert(0, "-ngl".to_string());
            args.insert(1, "all".to_string());
            summary = format!(
                "Using GPU with split-mode row — model partly in VRAM ({:.1} GB), partly in system memory",
                gpu_vram
            );
        }
    } else if has_openvino && has_npu {
        device = "CPU,GPU,NPU".to_string();
        ngl = "all".to_string();
        split_mode = "none".to_string();
        tps_est = 25.0;
        args.insert(0, "--device".to_string());
        args.insert(1, "CPU,GPU,NPU".to_string());
        summary = format!(
            "Using OpenVINO with CPU+GPU+NPU — simultaneous multi-device inference"
        );
    } else if gpu_vram >= 2.0 {
        let offload_layers = ((gpu_vram * 0.85 / model_size) * n_layers as f64).ceil() as u32;
        device = "gpu".to_string();
        ngl = offload_layers.to_string();
        tps_est = 30.0;
        args.insert(0, "-ngl".to_string());
        args.insert(1, offload_layers.to_string());
        summary = format!(
            "Offloading {} of {} layers to GPU ({:.1} GB usable / {:.1} GB model)",
            offload_layers, n_layers, gpu_vram * 0.85, model_size
        );
    } else {
        device = "cpu".to_string();
        ngl = "0".to_string();
        tps_est = 8.0;
        summary = format!(
            "CPU-only inference (no capable GPU detected)"
        );
    }

    let memory_adequate = sys.total_ram_gb >= total_needed_gb;

    Recommendation {
        device,
        n_gpu_layers: ngl,
        split_mode,
        main_gpu,
        tensor_split,
        args,
        summary,
        target_memory_gb: total_needed_gb,
        available_vram_gb: gpu_vram,
        model_size_gb: model_size,
        kv_cache_gb,
        memory_adequate,
        output_tokens_per_second_estimate: tps_est,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json_flag = args.iter().any(|a| a == "--json");
    let batch_flag = args.iter().any(|a| a == "--batch");
    let server_path = args
        .iter()
        .position(|a| a == "--server-path")
        .and_then(|i| args.get(i + 1).cloned());
    let state_path = args
        .iter()
        .position(|a| a == "--state-path")
        .and_then(|i| args.get(i + 1).cloned());

    let sys = probe_system_info();
    let gpus = probe_gpu();
    let npu = probe_npu();
    let backends = probe_backends(&server_path);
    let model = state_path.map(|s| read_model_info(&s)).flatten();
    let rec = compute_recommendation(&sys, &gpus, &npu, &backends, &model);

    let config = HardwareConfig {
        system: sys,
        gpu: gpus,
        npu,
        backends,
        model,
        recommendation: rec,
    };

    if batch_flag {
        println!("{}", config.recommendation.args.join(" "));
    } else if json_flag {
        println!("{}", serde_json::to_string_pretty(&config).unwrap());
    } else {
        print_summary(&config);
    }
}

fn print_summary(config: &HardwareConfig) {
    println!("=== System ===");
    println!(
        "  RAM: {:.1} GB total, {:.1} GB free",
        config.system.total_ram_gb, config.system.available_ram_gb
    );
    println!(
        "  CPU: {} logical / {} physical cores",
        config.system.logical_cores, config.system.physical_cores
    );

    println!("\n=== GPU ===");
    for gpu in &config.gpu {
        println!(
            "  {} — VRAM: {:.1} GB, Vulkan: {}",
            gpu.name, gpu.vram_gb, gpu.vulkan_available
        );
    }

    if let Some(npu) = &config.npu {
        println!("\n=== NPU ===");
        println!("  {} — driver: {}", npu.name, npu.driver_version);
    }

    println!("\n=== Backends ===");
    for b in &config.backends {
        println!(
            "  {}: {}",
            b.name,
            if b.available { "AVAILABLE" } else { "not found" }
        );
        for d in &b.devices {
            println!(
                "    └ {}",
                d
            );
        }
    }

    if let Some(m) = &config.model {
        println!("\n=== Model ===");
        println!("  {} ({:.1} GB)", m.display_name, m.size_gb);
    }

    println!("\n=== Recommendation ===");
    println!("  {}", config.recommendation.summary);
    println!("  Memory adequate: {}", config.recommendation.memory_adequate);
    println!("  Est. tokens/sec: {:.0}", config.recommendation.output_tokens_per_second_estimate);
    println!("  Launch args: {}", config.recommendation.args.join(" "));
}
