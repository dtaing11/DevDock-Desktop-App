//! What a change looks like, rendered where the checks ran — never on the
//! developer's own screen — and kept as PNGs the run's card can show.
//!
//! Four ways to a picture, in the order they are tried:
//!
//! 1. A **Flutter app's first frame**: a generated golden test calls the
//!    app's `main`, with the SDK's real fonts and the debug banner off.
//! 2. The **root widget** when `main` cannot start in a test (it waits on a
//!    service first): the widget `runApp(...)` is given, read out of
//!    `main.dart`, pumped directly.
//! 3. **Screens the agent names** in `.devdock/screens.json` — the widgets
//!    it changed, or the paths of a web app — because it knows which
//!    screen the change is on and the first frame usually is not it.
//! 4. A **web front end**, built and served inside the sandbox and
//!    photographed by a headless Chromium: a Flutter web build, a
//!    `package.json` with a build script, or a plain `index.html`.
//!
//! Everything generated is removed again and nothing is ever staged. A
//! capture that fails is a line in the log; the run goes on.

use std::path::{Path, PathBuf};

use crate::local_ci::runner::RunnerRegistry;
use crate::local_ci::{run_job_with, Job};

/// A screen the agent asked for: a Flutter widget expression with the
/// import it needs, or a path of a web app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Screen {
    pub name: String,
    pub widget: Option<String>,
    pub import: Option<String>,
    pub path: Option<String>,
}

/// Where the agent lists its screens, relative to the worktree.
pub const SCREENS_FILE: &str = ".devdock/screens.json";

/// What the agent is told about naming screens.
pub const SCREENS_NOTE: &str = "If your change is something a person would look at, write .devdock/screens.json \
    naming what to photograph, and DevDock renders it after the checks: \
    [{\"name\": \"settings\", \"widget\": \"SettingsPage()\", \"import\": \"package:app/settings.dart\"}] \
    for a Flutter widget (it is wrapped in a MaterialApp; give it any arguments it needs), or \
    [{\"name\": \"about\", \"path\": \"/about\"}] for a page of a web app. The file is never \
    committed. Without it the app's first screen is photographed.";

/// The screens listed in `.devdock/screens.json`, if any: a list, or an
/// object with a `screens` list.
pub fn declared_screens(root: &Path) -> Vec<Screen> {
    let Ok(text) = std::fs::read_to_string(root.join(SCREENS_FILE)) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let list = value.as_array().cloned().or_else(|| value.get("screens").and_then(|s| s.as_array()).cloned()).unwrap_or_default();
    list.iter()
        .filter_map(|s| {
            let text = |k: &str| s.get(k).and_then(|v| v.as_str()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let screen = Screen { name: text("name")?, widget: text("widget"), import: text("import"), path: text("path") };
            (screen.widget.is_some() || screen.path.is_some()).then_some(screen)
        })
        .take(8)
        .collect()
}

/// Flutter apps in the tree — a `pubspec.yaml` with the Flutter SDK and a
/// `lib/main.dart` — as (directory relative to the root, package name).
pub fn flutter_apps(root: &Path) -> Vec<(String, String)> {
    let mut apps = Vec::new();
    for dir in project_dirs(root) {
        let Ok(text) = std::fs::read_to_string(dir.join("pubspec.yaml")) else { continue };
        if !(text.contains("sdk: flutter") || text.contains("flutter:")) || !dir.join("lib/main.dart").exists() {
            continue;
        }
        let name = text.lines().find_map(|l| l.strip_prefix("name:")).map(|n| n.trim().trim_matches('"').trim_matches('\'').to_string());
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            apps.push((relative(root, &dir), name));
        }
    }
    apps.sort();
    apps
}

/// A web front end in the tree: how to build it and where its pages end
/// up, as (directory relative to the root, build command, output dir
/// relative to that directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTarget {
    pub dir: String,
    pub build: Option<String>,
    pub out: String,
}

pub fn web_targets(root: &Path) -> Vec<WebTarget> {
    let mut targets = Vec::new();
    for dir in project_dirs(root) {
        let rel = relative(root, &dir);
        if dir.join("web/index.html").exists() && dir.join("pubspec.yaml").exists() {
            targets.push(WebTarget { dir: rel, build: Some("flutter build web --release".into()), out: "build/web".into() });
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
            let has_build = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.pointer("/scripts/build").map(|b| b.is_string()))
                .unwrap_or(false);
            if has_build {
                let out = ["dist", "build", "out", ".output/public", "public"]
                    .into_iter()
                    .find(|d| dir.join(d).join("index.html").exists())
                    .unwrap_or("dist")
                    .to_string();
                targets.push(WebTarget { dir: rel, build: Some("npm run build".into()), out });
                continue;
            }
        }
        if dir.join("index.html").exists() && !dir.join("package.json").exists() {
            targets.push(WebTarget { dir: rel, build: None, out: ".".into() });
        }
    }
    targets
}

/// The widget `runApp(...)` is given in `main.dart`, as written, without
/// a leading `const`: what to pump when `main` itself cannot run.
pub fn root_widget_of(main_dart: &str) -> Option<String> {
    let start = main_dart.find("runApp(")? + "runApp(".len();
    let mut depth = 1usize;
    let mut end = None;
    for (i, c) in main_dart[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let expr = main_dart[start..end?].trim().trim_end_matches(',').trim();
    let expr = expr.strip_prefix("const ").unwrap_or(expr).trim();
    let expr: String = expr.split_whitespace().collect::<Vec<_>>().join(" ");
    (!expr.is_empty() && !expr.contains("await ")).then_some(expr)
}

/// Directories that hold a project file, nearest the root first, three
/// levels deep, skipping build trees and platform folders.
fn project_dirs(root: &Path) -> Vec<PathBuf> {
    const MARKERS: &[&str] = &["pubspec.yaml", "package.json", "index.html"];
    const SKIP: &[&str] = &["build", "node_modules", "ios", "android", "macos", "linux", "windows", "test", "dist", "out", ".dart_tool", "vendor", "target"];
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if MARKERS.iter().any(|m| dir.join(m).exists()) {
            found.push(dir);
            continue;
        }
        if depth >= 3 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && !SKIP.contains(&n)))
            .collect();
        children.sort();
        children.reverse();
        for child in children {
            stack.push((child, depth + 1));
        }
    }
    found.sort();
    found
}

fn relative(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default()
}

/// The golden test: one `testWidgets` per picture, each independent, so
/// one that cannot render does not stop the others.
pub fn smoke_test_source(package: &str, root_widget: Option<&str>, screens: &[Screen]) -> String {
    let mut imports = vec![format!("import 'package:{package}/main.dart' as app;"), format!("import 'package:{package}/main.dart';")];
    for s in screens {
        if let Some(import) = &s.import {
            let line = format!("import '{import}';");
            if !imports.contains(&line) {
                imports.push(line);
            }
        }
    }
    let mut tests = String::new();
    tests.push_str(
        r#"  testWidgets('devdock first frame', (tester) async {
    await _prepare(tester);
    await tester.runAsync(() async {
      try {
        final dynamic started = (app.main as dynamic)();
        if (started is Future) {
          await started.timeout(const Duration(seconds: 15));
        }
      } catch (_) {}
    });
    await _settle(tester);
    expect(find.byType(WidgetsApp), findsWidgets, reason: 'main put no WidgetsApp on screen');
    await expectLater(find.byType(WidgetsApp).first, matchesGoldenFile('devdock_first-frame.png'));
  });
"#,
    );
    if let Some(expr) = root_widget {
        tests.push_str(&format!(
            r#"  testWidgets('devdock root widget', (tester) async {{
    await _prepare(tester);
    await tester.runAsync(() async {{
      try {{
        await tester.pumpWidget({expr});
      }} catch (_) {{}}
    }});
    await _settle(tester);
    expect(find.byType(WidgetsApp), findsWidgets, reason: 'the root widget put no WidgetsApp on screen');
    await expectLater(find.byType(WidgetsApp).first, matchesGoldenFile('devdock_root-widget.png'));
  }});
"#
        ));
    }
    for s in screens {
        let Some(widget) = &s.widget else { continue };
        let name = slug(&s.name);
        tests.push_str(&format!(
            r#"  testWidgets('devdock screen {name}', (tester) async {{
    await _prepare(tester);
    await tester.runAsync(() async {{
      try {{
        await tester.pumpWidget(MaterialApp(debugShowCheckedModeBanner: false, home: {widget}));
      }} catch (_) {{}}
    }});
    await _settle(tester);
    await expectLater(find.byType(MaterialApp).first, matchesGoldenFile('devdock_screen-{name}.png'));
  }});
"#
        ));
    }
    format!(
        r#"// Generated by DevDock for screenshots; removed afterwards.
import 'dart:io';
import 'dart:typed_data';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
{imports}

Future<void> _loadFonts() async {{
  final root = Platform.environment['FLUTTER_ROOT'];
  if (root == null) return;
  final dir = '$root/bin/cache/artifacts/material_fonts';
  Future<void> load(String family, String file) async {{
    final f = File('$dir/$file');
    if (!await f.exists()) return;
    final bytes = await f.readAsBytes();
    final loader = FontLoader(family)..addFont(Future.value(ByteData.view(bytes.buffer)));
    await loader.load();
  }}
  await load('Roboto', 'Roboto-Regular.ttf');
  await load('Roboto', 'Roboto-Medium.ttf');
  await load('Roboto', 'Roboto-Bold.ttf');
  await load('MaterialIcons', 'MaterialIcons-Regular.otf');
}}

Future<void> _prepare(WidgetTester tester) async {{
  WidgetsApp.debugAllowBannerOverride = false;
  tester.view.physicalSize = const Size(1280, 800);
  tester.view.devicePixelRatio = 1.0;
  addTearDown(tester.view.reset);
  await tester.runAsync(() async {{
    try {{
      await _loadFonts();
    }} catch (_) {{}}
  }});
}}

Future<void> _settle(WidgetTester tester) async {{
  await tester.pump();
  try {{
    await tester.pumpAndSettle(const Duration(milliseconds: 100), EnginePhase.sendSemanticsUpdate, const Duration(seconds: 10));
  }} catch (_) {{}}
}}

void main() {{
{tests}}}
"#,
        imports = imports.join("\n"),
    )
}

fn slug(text: &str) -> String {
    let s: String = text.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.trim_matches('-').chars().take(40).collect::<String>();
    if s.is_empty() { "screen".into() } else { s }
}

/// Where kept screenshots go: DevDock's own directory, never the repository.
fn keep_dir() -> PathBuf {
    let dir = crate::secure_store::config_dir().join("screenshots");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Everything that can be photographed in the tree at `root`, in the
/// sandbox when `sandbox` is given (web pages need it: the browser lives
/// there). `label` names the files. Returns the PNGs kept.
pub fn capture(
    root: &Path,
    runners: &RunnerRegistry,
    sandbox: Option<&crate::sandbox::Sandbox>,
    label: &str,
    log: &mut dyn FnMut(String),
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let screens = declared_screens(root);
    if !screens.is_empty() {
        log(format!("screens named by the agent: {}", screens.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let runner = sandbox.map(|_| crate::sandbox::RUNNER_ID.to_string());
    let label = slug(label);

    for (rel, package) in flutter_apps(root) {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let test_dir = dir.join("test");
        let test_file = test_dir.join("devdock_smoke_test.dart");
        let root_widget = std::fs::read_to_string(dir.join("lib/main.dart")).ok().and_then(|m| root_widget_of(&m));
        let widget_screens: Vec<Screen> = screens.iter().filter(|s| s.widget.is_some()).cloned().collect();
        if std::fs::create_dir_all(&test_dir).is_err()
            || std::fs::write(&test_file, smoke_test_source(&package, root_widget.as_deref(), &widget_screens)).is_err()
        {
            continue;
        }
        let job = Job {
            name: if rel.is_empty() { "screenshots".into() } else { format!("screenshots ({rel})") },
            commands: vec!["flutter test --update-goldens test/devdock_smoke_test.dart".into()],
            dir: rel.clone(),
            runner: runner.clone(),
            timeout_secs: Some(600),
            ..Default::default()
        };
        log(format!("photographing {}", if rel.is_empty() { "the app".to_string() } else { rel.clone() }));
        let result = run_job_with(runners, root, &job);
        let _ = std::fs::remove_file(&test_file);
        // Whatever rendered, rendered: a test that failed took nothing, the
        // others still left their pictures.
        let mut got = 0;
        if let Ok(entries) = std::fs::read_dir(&test_dir) {
            let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("devdock_") && n.ends_with(".png"))).collect();
            files.sort();
            for file in files {
                let what = file.file_stem().and_then(|s| s.to_str()).unwrap_or("shot").trim_start_matches("devdock_").to_string();
                let dest = keep_dir().join(format!("{label}{}-{what}.png", if rel.is_empty() { String::new() } else { format!("-{}", slug(&rel)) }));
                if std::fs::copy(&file, &dest).is_ok() {
                    log(format!("screenshot ({what}): {}", dest.display()));
                    out.push(dest);
                    got += 1;
                }
                let _ = std::fs::remove_file(&file);
            }
        }
        if got == 0 {
            let why = result
                .output
                .lines()
                .rev()
                .find(|l| l.contains("Error") || l.contains("error") || l.contains("Exception"))
                .or_else(|| result.output.lines().rev().find(|l| !l.trim().is_empty()))
                .unwrap_or("no output")
                .trim();
            log(format!("no screenshot of {}: nothing rendered in a test ({})", package, why.chars().take(160).collect::<String>()));
        }
    }

    let web = web_targets(root);
    if !web.is_empty() {
        match sandbox {
            None => log("web pages are photographed in the sandbox only; none here".into()),
            Some(sandbox) => {
                let mut sandbox_log = |l: String| log(l);
                if let Err(e) = sandbox.provision(&["playwright"], &mut sandbox_log) {
                    log(format!("no web screenshots: {e}"));
                } else {
                    for target in web {
                        out.extend(capture_web(root, runners, sandbox, &target, &screens, &label, log));
                    }
                }
            }
        }
    }
    out
}

/// One web target: built, served on a local port inside the sandbox, and
/// each path photographed by headless Chromium.
fn capture_web(
    root: &Path,
    runners: &RunnerRegistry,
    sandbox: &crate::sandbox::Sandbox,
    target: &WebTarget,
    screens: &[Screen],
    label: &str,
    log: &mut dyn FnMut(String),
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let where_ = if target.dir.is_empty() { "the web app".to_string() } else { target.dir.clone() };
    if let Some(build) = &target.build {
        log(format!("building {where_}: {build}"));
        let job = Job {
            name: format!("web build ({where_})"),
            commands: vec![build.clone()],
            dir: target.dir.clone(),
            runner: Some(crate::sandbox::RUNNER_ID.into()),
            timeout_secs: Some(900),
            ..Default::default()
        };
        let result = run_job_with(runners, root, &job);
        if !result.ok {
            let last = result.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            log(format!("no web screenshots of {where_}: the build failed ({})", last.chars().take(160).collect::<String>()));
            return out;
        }
    }
    let mut paths: Vec<(String, String)> = screens.iter().filter_map(|s| s.path.clone().map(|p| (slug(&s.name), p))).collect();
    if paths.is_empty() {
        paths.push(("home".into(), "/".into()));
    }
    let shots_dir = root.join(".devdock/shots");
    let _ = std::fs::create_dir_all(&shots_dir);
    let inner = sandbox.inner_root();
    let serve_dir = if target.dir.is_empty() { format!("{inner}/{}", target.out) } else { format!("{inner}/{}/{}", target.dir, target.out) };
    let mut script = format!(
        "cd {} && (python3 -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1 &) && sleep 1.5",
        shell_quote(&serve_dir)
    );
    for (name, path) in &paths {
        let out_file = format!("{inner}/.devdock/shots/{name}.png");
        let path = if path.starts_with('/') { path.clone() } else { format!("/{path}") };
        script.push_str(&format!(
            " && (cd \"$HOME/.devdock-playwright\" && npx playwright screenshot --browser chromium --viewport-size 1280,800 --wait-for-timeout 1500 {} {} >/dev/null 2>&1 || true)",
            shell_quote(&format!("http://127.0.0.1:8765{path}")),
            shell_quote(&out_file)
        ));
    }
    script.push_str("; pkill -f 'http.server 8765' >/dev/null 2>&1 || true");
    log(format!("photographing {where_} at {}", paths.iter().map(|(_, p)| p.as_str()).collect::<Vec<_>>().join(", ")));
    let result = sandbox.exec(&script, "", &[], Some(std::time::Duration::from_secs(300)));
    if let Err(e) = result {
        log(format!("no web screenshots of {where_}: {e}"));
    }
    for (name, _) in &paths {
        let file = shots_dir.join(format!("{name}.png"));
        if file.exists() {
            let dest = keep_dir().join(format!("{label}-web-{name}.png"));
            if std::fs::copy(&file, &dest).is_ok() {
                log(format!("screenshot (web {name}): {}", dest.display()));
                out.push(dest);
            }
        } else {
            log(format!("no screenshot of {where_} {name}: the page did not render"));
        }
    }
    let _ = std::fs::remove_dir_all(&shots_dir);
    if let Ok(mut entries) = std::fs::read_dir(root.join(".devdock")) {
        if entries.next().is_none() {
            let _ = std::fs::remove_dir(root.join(".devdock"));
        }
    }
    out
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_web_targets_and_screens_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("mobile/lib")).unwrap();
        std::fs::create_dir_all(root.join("mobile/web")).unwrap();
        std::fs::write(root.join("mobile/pubspec.yaml"), "name: farm_app\ndependencies:\n  flutter:\n    sdk: flutter\n").unwrap();
        std::fs::write(root.join("mobile/lib/main.dart"), "void main() async {\n  await init();\n  runApp(\n    const FarmApp(theme: light),\n  );\n}\n").unwrap();
        std::fs::write(root.join("mobile/web/index.html"), "<html></html>").unwrap();
        std::fs::create_dir_all(root.join("site")).unwrap();
        std::fs::write(root.join("site/package.json"), r#"{"scripts": {"build": "vite build"}}"#).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/index.html"), "<h1>hi</h1>").unwrap();
        std::fs::create_dir_all(root.join(".devdock")).unwrap();
        std::fs::write(root.join(SCREENS_FILE), r#"{"screens": [{"name": "Settings page", "widget": "SettingsPage()", "import": "package:farm_app/settings.dart"}, {"name": "about", "path": "/about"}, {"name": "empty"}]}"#).unwrap();

        assert_eq!(flutter_apps(root), [("mobile".to_string(), "farm_app".to_string())]);
        let web = web_targets(root);
        assert_eq!(web.len(), 3, "{web:?}");
        assert!(web.iter().any(|t| t.dir == "mobile" && t.build.as_deref() == Some("flutter build web --release") && t.out == "build/web"));
        assert!(web.iter().any(|t| t.dir == "site" && t.build.as_deref() == Some("npm run build") && t.out == "dist"));
        assert!(web.iter().any(|t| t.dir == "docs" && t.build.is_none() && t.out == "."));
        let screens = declared_screens(root);
        assert_eq!(screens.len(), 2, "an entry with neither widget nor path is dropped: {screens:?}");
        assert_eq!(screens[0].widget.as_deref(), Some("SettingsPage()"));
        assert_eq!(screens[1].path.as_deref(), Some("/about"));

        let main = std::fs::read_to_string(root.join("mobile/lib/main.dart")).unwrap();
        assert_eq!(root_widget_of(&main).as_deref(), Some("FarmApp(theme: light)"));
        assert_eq!(root_widget_of("void main() {}"), None);
        assert_eq!(root_widget_of("runApp(MyApp())"), Some("MyApp()".into()));

        let source = smoke_test_source("farm_app", Some("FarmApp(theme: light)"), &screens);
        assert!(source.contains("import 'package:farm_app/settings.dart';"));
        assert!(source.contains("matchesGoldenFile('devdock_first-frame.png')"));
        assert!(source.contains("tester.pumpWidget(FarmApp(theme: light))"));
        assert!(source.contains("home: SettingsPage()") && source.contains("devdock_screen-settings-page.png"));
        assert!(source.contains("Roboto-Regular.ttf"));
        assert_eq!(slug("Settings page!"), "settings-page");
    }

    /// A real Flutter app inside the sandbox: `main` waits on something a
    /// test cannot provide, so the first frame fails, the root widget
    /// renders, and a screen the agent named renders too.
    /// `cargo test --lib screenshots::tests::live_flutter -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_flutter_root_widget_and_named_screen_render_in_the_sandbox() {
        if crate::sandbox::installed().is_empty() {
            eprintln!("no sandbox runtime; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("pubspec.yaml"), "name: hello_app\nenvironment:\n  sdk: ^3.0.0\ndependencies:\n  flutter:\n    sdk: flutter\ndev_dependencies:\n  flutter_test:\n    sdk: flutter\nflutter:\n  uses-material-design: true\n").unwrap();
        // main waits on a "service" forever before drawing.
        std::fs::write(root.join("lib/main.dart"), "import 'dart:async';\nimport 'package:flutter/material.dart';\nimport 'settings.dart';\n\nFuture<void> main() async {\n  await Completer<void>().future;\n  runApp(const HelloApp());\n}\n\nclass HelloApp extends StatelessWidget {\n  const HelloApp({super.key});\n  @override\n  Widget build(BuildContext context) => MaterialApp(home: Scaffold(appBar: AppBar(title: const Text('Root widget')), body: const Center(child: Text('Rendered without main.'))));\n}\n").unwrap();
        std::fs::write(root.join("lib/settings.dart"), "import 'package:flutter/material.dart';\n\nclass SettingsPage extends StatelessWidget {\n  const SettingsPage({super.key});\n  @override\n  Widget build(BuildContext context) => Scaffold(appBar: AppBar(title: const Text('Settings')), body: ListView(children: const [SwitchListTile(value: true, onChanged: null, title: Text('Dark mode')), ListTile(title: Text('Account'), subtitle: Text('dina@example.com'))]));\n}\n").unwrap();
        std::fs::create_dir_all(root.join(".devdock")).unwrap();
        std::fs::write(root.join(SCREENS_FILE), r#"[{"name": "settings", "widget": "SettingsPage()", "import": "package:hello_app/settings.dart"}]"#).unwrap();
        let mut log = |l: String| println!("  {l}");
        let sandbox = std::sync::Arc::new(crate::sandbox::Sandbox::start(&crate::sandbox::Spec::default(), root, &mut log).unwrap());
        sandbox.provision(&["flutter"], &mut log).unwrap();
        let mut runners = RunnerRegistry::with_builtins();
        runners.register(Box::new(crate::sandbox::SandboxRunner(sandbox.clone())));
        for mut step in crate::local_ci::prepare_jobs(root, true) {
            step.runner = Some(crate::sandbox::RUNNER_ID.into());
            assert!(run_job_with(&runners, root, &step).ok);
        }
        let shots = capture(root, &runners, Some(&sandbox), "live-deep", &mut log);
        let names: Vec<String> = shots.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        println!("{names:?}");
        assert!(names.iter().any(|n| n.contains("root-widget")), "the root widget rendered although main hangs: {names:?}");
        assert!(names.iter().any(|n| n.contains("screen-settings")), "the named screen rendered: {names:?}");
        assert!(!names.iter().any(|n| n.contains("first-frame")), "main never drew, so no first frame: {names:?}");
        assert!(!root.join("test/devdock_smoke_test.dart").exists());
    }

    /// A plain web page photographed by the sandbox's headless browser.
    /// `cargo test --lib screenshots::tests::live_web -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_web_page_is_photographed_in_the_sandbox() {
        if crate::sandbox::installed().is_empty() {
            eprintln!("no sandbox runtime; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("index.html"), "<html><body style='font-family:sans-serif;background:#f4f4ff'><h1 style='color:#334'>DevDock web capture</h1><p>Photographed by headless Chromium inside the sandbox.</p><a href='/about.html'>about</a></body></html>").unwrap();
        std::fs::write(root.join("about.html"), "<html><body><h1>About</h1><p>A second page the agent named.</p></body></html>").unwrap();
        std::fs::create_dir_all(root.join(".devdock")).unwrap();
        std::fs::write(root.join(SCREENS_FILE), r#"[{"name": "about", "path": "/about.html"}]"#).unwrap();
        let mut log = |l: String| println!("  {l}");
        let sandbox = std::sync::Arc::new(crate::sandbox::Sandbox::start(&crate::sandbox::Spec::default(), root, &mut log).unwrap());
        let mut runners = RunnerRegistry::with_builtins();
        runners.register(Box::new(crate::sandbox::SandboxRunner(sandbox.clone())));
        let shots = capture(root, &runners, Some(&sandbox), "live-web", &mut log);
        let names: Vec<String> = shots.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        println!("{names:?}");
        assert!(names.iter().any(|n| n.contains("web-about")), "{names:?}");
        let bytes = std::fs::read(&shots[0]).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (1280, 800));
        assert!(!root.join(".devdock/shots").exists(), "generated files are gone");
    }
}
