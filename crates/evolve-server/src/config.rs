pub const EMBEDDED_DEFAULT_CONFIG: &str = include_str!("../../../config/evo-router.default.toml");

pub fn load_config(explicit: Option<&std::path::Path>) -> anyhow::Result<(evolve_core::config::FileConfig, Option<std::path::PathBuf>)> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = explicit {
        // 显式指定的配置文件不存在 = 用户敲错了路径，静默回退默认配置
        // 会让网关看似正常启动实则路由全错（审查 M-8）——必须报错
        if !p.is_file() {
            anyhow::bail!("explicit config not found: {}", p.display());
        }
        candidates.push(p.to_path_buf());
    }
    candidates.extend(evolve_core::config::default_config_paths());
    let (mut cfg, path) = evolve_core::config::FileConfig::from_paths(&candidates, EMBEDDED_DEFAULT_CONFIG)?;
    // 数据目录在此定稿（含 ~/.evolve → ~/.evo-router 的一次性迁移）：
    // 下游一律直接用 config.data.dir，不再各自 shellexpand（免得迁移只生效一半）
    cfg.data.dir = evolve_core::config::resolve_data_dir(&cfg.data.dir)
        .to_string_lossy()
        .into_owned();
    Ok((cfg, path))
}
