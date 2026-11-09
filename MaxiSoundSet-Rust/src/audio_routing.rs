//! Exact endpoint selection and role plans. No audio or Windows mutations here.
use crate::{core_audio::{DeviceInfo, RouteRestore}, settings::Settings};
use anyhow::{Context, Result};

pub fn initial_source(devices: &[DeviceInfo]) -> Option<String> {
    let known: Vec<_> = devices.iter().filter(|d| d.vb_cable).collect();
    if let Some(current) = known.iter().find(|d| d.system_default) {
        return Some(current.id.clone());
    }
    if known.len() == 1 { return Some(known[0].id.clone()); }
    if !known.is_empty() { return None; }
    let virtuals: Vec<_> = devices.iter().filter(|d| d.virtual_input).collect();
    (virtuals.len() == 1).then(|| virtuals[0].id.clone())
}

pub fn guidance(english: bool) -> &'static str {
    if english {
        "Select your playback device in this app, enable Automatic routing if Windows uses the cable, then Run."
    } else {
        "دستگاه پخش را در همین برنامه انتخاب کن؛ اگر خروجی ویندوز کابل است، «هدایت خودکار» را روشن کن و اجرا را بزن."
    }
}

pub fn selected_cable(s: &Settings, devices: &[DeviceInfo], id: &str) -> bool {
    id == s.source_id && devices.iter().any(|d| d.id == id && d.vb_cable)
}

pub fn playback<'a>(devices: &'a [DeviceInfo], id: &str) -> Option<&'a DeviceInfo> {
    devices.iter().find(|d| d.id == id && !d.virtual_input)
}

pub fn resolve_output(s: &Settings, devices: &[DeviceInfo], current: Option<&str>) -> Result<String> {
    let message = guidance(s.english);
    if !s.output_id.is_empty() {
        anyhow::ensure!(playback(devices, &s.output_id).is_some(), "{message}");
        return Ok(s.output_id.clone());
    }
    let current = current.context(message)?;
    if playback(devices, current).is_some() { return Ok(current.into()); }
    // A name, another virtual endpoint, or a missing remembered endpoint is not
    // evidence of where the selected cable should be heard.
    anyhow::ensure!(selected_cable(s, devices, current), "{message}");
    anyhow::ensure!(s.mode == 1 || s.auto_route, "{message}");
    let remembered = playback(devices, &s.last_physical_output_id)
        .filter(|d| d.verified_physical).context(message)?;
    Ok(remembered.id.clone())
}

pub fn windows_routes(s: &Settings, devices: &[DeviceInfo], defaults: &[String], output: &str) -> Result<Vec<RouteRestore>> {
    // Explicit playback supports externally forwarded/manual configurations.
    // Only System default depends on where the multimedia role currently points.
    if s.output_id.is_empty() {
        let current = defaults.get(1).context(guidance(s.english))?;
        anyhow::ensure!(playback(devices, current).is_some() || (s.auto_route && selected_cable(s, devices, current)), "{}", guidance(s.english));
    }
    if !s.auto_route { return Ok(Vec::new()); }
    Ok(defaults.iter().enumerate().filter(|(_, id)| selected_cable(s, devices, id))
        .map(|(role, id)| RouteRestore { role: role as i32, original_id: id.clone(), expected_id: output.into() }).collect())
}

pub fn full_routes(s: &Settings, defaults: &[String]) -> Vec<RouteRestore> {
    if !s.auto_route { return Vec::new(); }
    defaults.iter().enumerate().map(|(role, id)| RouteRestore {
        role: role as i32, original_id: id.clone(), expected_id: s.source_id.clone(),
    }).collect()
}

pub fn committed_full_routes(routes: &[RouteRestore], source: &str, output: &str) -> Vec<RouteRestore> {
    routes.iter().cloned().map(|mut route| {
        // Preserve Full's established orphan-cable policy after startup succeeds:
        // a role already on this cable returns to the selected playback output.
        if route.original_id == source { route.original_id = output.into(); }
        route
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device(id: &str, virtual_input: bool, vb_cable: bool, verified_physical: bool) -> DeviceInfo {
        DeviceInfo { id: id.into(), name: id.into(), virtual_input, vb_cable, verified_physical, system_default: false }
    }
    fn inventory() -> Vec<DeviceInfo> {
        vec![device("speaker", false, false, true), device("headphones", false, false, true),
            device("cable", true, true, false), device("other-virtual", true, false, false)]
    }
    #[test]
    fn known_cable_requires_exact_verified_hint_and_preserves_explicit_intent() {
        let mut s = Settings { source_id: "cable".into(), mode: 1, ..Settings::default() };
        let mut all = inventory();
        assert!(resolve_output(&s, &all, Some("cable")).is_err());
        s.last_physical_output_id = "speaker".into();
        assert_eq!(resolve_output(&s, &all, Some("cable")).unwrap(), "speaker");
        assert!(resolve_output(&s, &all, Some("other-virtual")).is_err());
        assert!(resolve_output(&s, &all, Some("missing")).is_err());
        all[0].verified_physical = false;
        assert!(resolve_output(&s, &all, Some("cable")).is_err());
        s.output_id = "speaker".into();
        assert_eq!(resolve_output(&s, &all, None).unwrap(), "speaker", "Explicit Unknown hardware remains usable");
        s.output_id = "disconnected".into();
        assert!(resolve_output(&s, &all, Some("headphones")).is_err());
    }
    #[test]
    fn initial_source_prefers_official_identity_and_leaves_ambiguity_visible() {
        let mut all = inventory();
        all.reverse();
        assert_eq!(initial_source(&all).as_deref(), Some("cable"));
        all.push(device("second-cable", true, true, false));
        assert_eq!(initial_source(&all), None);
        all.last_mut().unwrap().system_default = true;
        assert_eq!(initial_source(&all).as_deref(), Some("second-cable"));
    }
    #[test]
    fn windows_plan_changes_only_matching_cable_roles_and_manual_off_never_routes() {
        let mut s = Settings { source_id: "cable".into(), output_id: "speaker".into(), ..Settings::default() };
        let defaults = vec!["headphones".into(), "cable".into(), "other-virtual".into()];
        let routes = windows_routes(&s, &inventory(), &defaults, "speaker").unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].role, 1);
        s.auto_route = false;
        assert!(windows_routes(&s, &inventory(), &defaults, "speaker").unwrap().is_empty());
        assert!(windows_routes(&s, &inventory(), &[], "speaker").unwrap().is_empty(), "Explicit manual playback has no required default query");
        s.output_id.clear();
        assert!(windows_routes(&s, &inventory(), &defaults, "speaker").is_err());
        s.output_id = "speaker".into();
        assert!(full_routes(&s, &defaults).is_empty());
        let physical = vec!["cable".into(), "headphones".into(), "cable".into()];
        assert!(windows_routes(&s, &inventory(), &physical, "speaker").unwrap().is_empty());
        s.auto_route = true;
        assert_eq!(windows_routes(&s, &inventory(), &physical, "speaker").unwrap().iter().map(|r| r.role).collect::<Vec<_>>(), vec![0, 2]);
        let other = vec!["headphones".into(), "other-virtual".into(), "speaker".into()];
        assert!(windows_routes(&s, &inventory(), &other, "speaker").unwrap().is_empty(), "An explicit externally forwarded output remains usable");
    }
    #[test]
    fn full_startup_rollback_and_successful_baseline_are_distinct() {
        let s = Settings { source_id: "cable".into(), ..Settings::default() };
        let defaults = vec!["headphones".into(), "cable".into(), "other-virtual".into()];
        let startup = full_routes(&s, &defaults);
        assert_eq!(startup[1].original_id, "cable");
        let committed = committed_full_routes(&startup, "cable", "speaker");
        assert_eq!(committed[1].original_id, "speaker");
        assert_eq!(committed[0].original_id, "headphones");
        assert_eq!(committed[2].original_id, "other-virtual");
    }

    /// Opt-in read-only evidence. No endpoints are opened and no settings,
    /// defaults, volumes or audio streams are changed.
    #[test]
    #[ignore]
    fn native_routing_inventory_evidence() {
        let directory = std::path::PathBuf::from(std::env::var_os("MAXI_ROUTE_EVIDENCE_DIR").expect("Set MAXI_ROUTE_EVIDENCE_DIR"));
        let _com = crate::core_audio::ComScope::new().unwrap();
        // Match background discovery: devices() owns its complete COM lifetime.
        let devices = std::thread::spawn(crate::core_audio::devices).join().unwrap().unwrap();
        let defaults: Vec<_> = (0..3).map(|role| crate::core_audio::default_id(role).unwrap()).collect();
        assert!(devices.iter().any(|d| d.id == defaults[1] && d.system_default), "Fresh-thread scan must label the current multimedia default");
        let source = initial_source(&devices).unwrap_or_default();
        let hint = devices.iter().find(|d| d.verified_physical && !d.virtual_input).map(|d| d.id.clone()).unwrap_or_default();
        let decisions: Vec<_> = [0, 1].into_iter().flat_map(|mode| [false, true].into_iter().map(move |auto_route| (mode, auto_route)))
            .map(|(mode, auto_route)| {
                let s = Settings { mode, auto_route, source_id: source.clone(), last_physical_output_id: hint.clone(), ..Settings::default() };
                let resolved = resolve_output(&s, &devices, Some(&defaults[1]));
                let plan = resolved.as_ref().map(|output| if mode == 0 { windows_routes(&s, &devices, &defaults, output).map(|p| p.len()) } else { Ok(full_routes(&s, &defaults).len()) });
                serde_json::json!({"mode": mode, "automatic": auto_route, "output": resolved.as_ref().ok(), "error": resolved.as_ref().err().map(|e| format!("{e:#}")), "planned_roles": format!("{plan:?}")})
            }).collect();
        let endpoints: Vec<_> = devices.iter().map(|d| serde_json::json!({"id":d.id,"name":d.name,"virtual":d.virtual_input,"official_vb":d.vb_cable,"verified_physical":d.verified_physical,"multimedia_default":d.system_default})).collect();
        std::fs::create_dir_all(&directory).unwrap();
        crate::settings::write_json(&directory.join("native-routing-inventory.json"), &serde_json::json!({"read_only":true,"defaults":defaults,"endpoints":endpoints,"initial_source":source,"test_hint":hint,"decisions":decisions})).unwrap();
    }
}
