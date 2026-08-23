use anyhow::{Context, ensure};
use codex_tools_runtime::contracts::ExecCommandOutput;
use rmcp::model::{CallToolRequestParams, ClientInfo, JsonObject};
use rmcp::service::RunningService;
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE_REPOSITORY: &str = "fixture/skills";
const INSTALLER: &str = "python3 \"$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py\"";

pub struct InstallerFixture {
    workspace: PathBuf,
    codex_home: PathBuf,
    tmp_dir: PathBuf,
    git_config: PathBuf,
}

impl InstallerFixture {
    pub fn prepare(
        profile: &Path,
        workspace: &Path,
        home: &Path,
        path_shadow: &Path,
    ) -> anyhow::Result<Self> {
        let source = profile.join("installer source repository");
        let codex_home = home.join(".codex");
        let tmp_dir = profile.join("installer tmp");
        let git_config = profile.join("installer.gitconfig");
        for directory in [&source, &codex_home, &tmp_dir] {
            fs::create_dir_all(directory)
                .with_context(|| format!("create installer fixture {}", directory.display()))?;
        }

        for (name, valid) in [
            ("global-ok", true),
            ("project-ok", true),
            ("yielded-ok", true),
            ("partial-ok", true),
            ("missing-skill-md", false),
            ("interrupted", true),
            ("recreated-global", true),
            ("recreated-project", true),
        ] {
            let skill = source.join("skills").join(name);
            fs::create_dir_all(&skill)?;
            if valid {
                fs::write(
                    skill.join("SKILL.md"),
                    format!(
                        "---\nname: {name}\ndescription: Packaged installer fixture {name}\n---\n{name} body\n"
                    ),
                )?;
            } else {
                fs::write(skill.join("README.md"), "missing SKILL.md fixture\n")?;
            }
        }
        run_git(&source, &["init", "--initial-branch=main"])?;
        run_git(&source, &["config", "user.name", "Installer Fixture"])?;
        run_git(
            &source,
            &["config", "user.email", "installer-fixture@example.invalid"],
        )?;
        run_git(&source, &["add", "."])?;
        run_git(&source, &["commit", "-m", "fixture skills"])?;

        let source_url = format!("file://{}", source.display());
        run_git_config(&git_config, &["protocol.file.allow", "always"])?;
        run_git_config(
            &git_config,
            &[
                &format!("url.{source_url}.insteadOf"),
                "https://github.com/fixture/skills.git",
            ],
        )?;

        let git_shim = path_shadow.join("git");
        fs::write(
            &git_shim,
            "#!/bin/sh\nif [ \"${1-}\" = clone ] && [ -n \"${MCP_INSTALLER_GIT_DELAY-}\" ]; then\n  sleep \"$MCP_INSTALLER_GIT_DELAY\"\nfi\nexec /usr/bin/git \"$@\"\n",
        )?;
        crate::package::set_executable(&git_shim)?;

        Ok(Self {
            workspace: workspace.to_path_buf(),
            codex_home,
            tmp_dir,
            git_config,
        })
    }

    #[must_use]
    pub fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    #[must_use]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    #[must_use]
    pub fn git_config(&self) -> &Path {
        &self.git_config
    }

    pub async fn run(&self, endpoint: &str) -> anyhow::Result<()> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned()),
        );
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .context("connect packaged installer workflow client")?;

        verify_system_installer(&client).await?;
        self.verify_installations(&client).await?;
        client.cancel().await?;
        Ok(())
    }

    async fn verify_installations(
        &self,
        client: &RunningService<RoleClient, ClientInfo>,
    ) -> anyhow::Result<()> {
        let bridge = exec(
            client,
            "test -x \"$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer/scripts/install-skill-from-github.py\" && printf bridge-ready",
            10_000,
            false,
        )
        .await?;
        assert_completed(&bridge, 0, "bridge-ready")?;

        let global = exec(
            client,
            &install_command(&["global-ok"], None, None),
            10_000,
            false,
        )
        .await?;
        assert_completed(&global, 0, "Installed global-ok to ")?;
        assert_output(
            &global,
            &format!(
                "Installed global-ok to {}\n",
                self.codex_home.join("skills/global-ok").display()
            ),
        )?;
        assert_skill(client, "global", "global-ok", "global-ok body").await?;

        let project_root = self.workspace.join(".agents/skills");
        let project = exec(
            client,
            &install_command(&["project-ok"], Some(&project_root), None),
            10_000,
            false,
        )
        .await?;
        assert_completed(&project, 0, "Installed project-ok to ")?;
        assert_output(
            &project,
            &format!(
                "Installed project-ok to {}\n",
                project_root.join("project-ok").display()
            ),
        )?;
        assert_skill(client, "project", "project-ok", "project-ok body").await?;

        let collision = exec(
            client,
            &install_command(&["global-ok"], None, None),
            10_000,
            false,
        )
        .await?;
        assert_completed(&collision, 1, "Destination already exists:")?;
        assert_output(
            &collision,
            &format!(
                "Error: Destination already exists: {}\n",
                self.codex_home.join("skills/global-ok").display()
            ),
        )?;
        assert_skill(client, "global", "global-ok", "global-ok body").await?;

        let yielded = exec(
            client,
            &install_command(&["yielded-ok"], None, Some("1")),
            100,
            false,
        )
        .await?;
        let yielded_session = yielded
            .session_id
            .context("delayed installer did not yield a live session")?;
        ensure!(
            yielded.exit_code.is_none(),
            "yielded installer exited early"
        );
        let yielded = write_stdin(client, yielded_session, "", 10_000).await?;
        assert_completed(&yielded, 0, "Installed yielded-ok to ")?;
        assert_output(
            &yielded,
            &format!(
                "Installed yielded-ok to {}\n",
                self.codex_home.join("skills/yielded-ok").display()
            ),
        )?;
        assert_skill(client, "global", "yielded-ok", "yielded-ok body").await?;

        let partial = exec(
            client,
            &install_command(
                &["partial-ok", "missing-skill-md"],
                Some(&project_root),
                None,
            ),
            10_000,
            false,
        )
        .await?;
        assert_completed(
            &partial,
            1,
            "SKILL.md not found in selected skill directory.",
        )?;
        assert_output(
            &partial,
            "Error: SKILL.md not found in selected skill directory.\n",
        )?;
        assert_skill(client, "project", "partial-ok", "partial-ok body").await?;
        assert_skill_absent(client, "project", "missing-skill-md").await?;

        let interrupted = exec(
            client,
            &install_command(&["interrupted"], None, Some("30")),
            100,
            false,
        )
        .await?;
        let interrupted_session = interrupted
            .session_id
            .context("interruptible installer did not yield a live session")?;
        let interrupted = write_stdin(client, interrupted_session, "\u{3}", 10_000).await?;
        ensure!(
            interrupted.exit_code.is_some_and(|code| code != 0),
            "interrupted installer unexpectedly succeeded: {interrupted:?}"
        );
        assert_skill_absent(client, "global", "interrupted").await?;
        self.assert_temp_cleanup(client).await?;

        let replace_roots = format!(
            "rm -rf {} {} && mkdir -p {} {}",
            shell_quote(&self.codex_home.join("skills")),
            shell_quote(&project_root),
            shell_quote(&self.codex_home.join("skills")),
            shell_quote(&project_root),
        );
        assert_completed(&exec(client, &replace_roots, 10_000, false).await?, 0, "")?;
        assert_skill_absent(client, "global", "global-ok").await?;
        assert_skill_absent(client, "project", "project-ok").await?;

        assert_completed(
            &exec(
                client,
                &install_command(&["recreated-global"], None, None),
                10_000,
                false,
            )
            .await?,
            0,
            "Installed recreated-global to ",
        )?;
        assert_completed(
            &exec(
                client,
                &install_command(&["recreated-project"], Some(&project_root), None),
                10_000,
                false,
            )
            .await?,
            0,
            "Installed recreated-project to ",
        )?;
        assert_skill(
            client,
            "global",
            "recreated-global",
            "recreated-global body",
        )
        .await?;
        assert_skill(
            client,
            "project",
            "recreated-project",
            "recreated-project body",
        )
        .await?;
        self.assert_temp_cleanup(client).await
    }

    async fn assert_temp_cleanup(
        &self,
        client: &RunningService<RoleClient, ClientInfo>,
    ) -> anyhow::Result<()> {
        let cleanup = exec(
            client,
            "if [ -d \"$TMPDIR/codex\" ]; then find \"$TMPDIR/codex\" -mindepth 1 -maxdepth 1 -name 'skill-install-*' -print; fi",
            10_000,
            false,
        )
        .await?;
        ensure!(
            cleanup.exit_code == Some(0) && cleanup.output.is_empty(),
            "installer temporary checkout survived cleanup: {cleanup:?}"
        );
        Ok(())
    }
}

async fn verify_system_installer(
    client: &RunningService<RoleClient, ClientInfo>,
) -> anyhow::Result<()> {
    let listed = call(client, "skills.list", json!({"scope": "system"})).await?;
    let skills = listed["skills"]
        .as_array()
        .context("system skills.list omitted skills")?;
    ensure!(
        skills.len() == 1,
        "unexpected system skill catalog: {skills:?}"
    );
    ensure!(
        skills[0]["scope"] == "system"
            && skills[0]["package"] == "skill-installer"
            && skills[0]["main_resource"] == "skill://host/system/skill-installer/SKILL.md",
        "packaged installer did not use the exact reserved system handle: {}",
        skills[0]
    );
    let read = call(
        client,
        "skills.read",
        json!({
            "scope": "system",
            "package": "skill-installer",
            "resource": "skill://host/system/skill-installer/SKILL.md"
        }),
    )
    .await?;
    let contents = read["contents"]
        .as_str()
        .context("system skills.read omitted contents")?;
    ensure!(
        contents.contains("name: skill-installer")
            && contents.contains("$MCP_AGENT_SYSTEM_SKILLS_ROOT/skill-installer"),
        "system installer instructions omitted the packaged bridge"
    );
    Ok(())
}

async fn assert_skill(
    client: &RunningService<RoleClient, ClientInfo>,
    scope: &str,
    package: &str,
    expected_body: &str,
) -> anyhow::Result<()> {
    let listed = call(client, "skills.list", json!({"scope": scope})).await?;
    let skills = listed["skills"]
        .as_array()
        .context("skills.list omitted skills")?;
    ensure!(
        skills.iter().any(|skill| {
            skill["scope"] == scope
                && skill["package"] == package
                && skill["main_resource"] == format!("skill://host/{scope}/{package}/SKILL.md")
        }),
        "{scope} skill {package} was not discoverable: {skills:?}"
    );
    let resource = format!("skill://host/{scope}/{package}/SKILL.md");
    let read = call(
        client,
        "skills.read",
        json!({"scope": scope, "package": package, "resource": resource}),
    )
    .await?;
    ensure!(
        read["contents"]
            .as_str()
            .is_some_and(|contents| contents.contains(expected_body)),
        "{scope} skill {package} was not readable: {read}"
    );
    Ok(())
}

async fn assert_skill_absent(
    client: &RunningService<RoleClient, ClientInfo>,
    scope: &str,
    package: &str,
) -> anyhow::Result<()> {
    let listed = call(client, "skills.list", json!({"scope": scope})).await?;
    let skills = listed["skills"]
        .as_array()
        .context("skills.list omitted skills")?;
    ensure!(
        skills.iter().all(|skill| skill["package"] != package),
        "{scope} skill {package} unexpectedly remained discoverable: {skills:?}"
    );
    Ok(())
}

async fn exec(
    client: &RunningService<RoleClient, ClientInfo>,
    cmd: &str,
    yield_time_ms: u64,
    tty: bool,
) -> anyhow::Result<ExecCommandOutput> {
    let value = call(
        client,
        "exec_command",
        json!({
            "cmd": cmd,
            "shell": "/bin/sh",
            "login": false,
            "tty": tty,
            "yield_time_ms": yield_time_ms,
            "max_output_tokens": 10_000
        }),
    )
    .await?;
    serde_json::from_value(value).context("decode exec_command result")
}

async fn write_stdin(
    client: &RunningService<RoleClient, ClientInfo>,
    session_id: i32,
    chars: &str,
    yield_time_ms: u64,
) -> anyhow::Result<ExecCommandOutput> {
    let value = call(
        client,
        "write_stdin",
        json!({
            "session_id": session_id,
            "chars": chars,
            "yield_time_ms": yield_time_ms,
            "max_output_tokens": 10_000
        }),
    )
    .await?;
    serde_json::from_value(value).context("decode write_stdin result")
}

async fn call(
    client: &RunningService<RoleClient, ClientInfo>,
    name: &'static str,
    arguments: Value,
) -> anyhow::Result<Value> {
    let arguments: JsonObject = arguments
        .as_object()
        .context("tool arguments must be an object")?
        .clone();
    let result = client
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .with_context(|| format!("call packaged {name}"))?;
    ensure!(
        result.is_error != Some(true),
        "packaged {name} returned a tool error: {:?}",
        result.structured_content
    );
    result
        .structured_content
        .context("packaged tool omitted structured content")
}

fn install_command(skills: &[&str], destination: Option<&Path>, delay: Option<&str>) -> String {
    let mut command = delay.map_or_else(String::new, |seconds| {
        format!(
            "MCP_INSTALLER_GIT_DELAY={} ",
            shell_quote(Path::new(seconds))
        )
    });
    write!(command, "{INSTALLER} --repo {FIXTURE_REPOSITORY} --path")
        .expect("writing to String cannot fail");
    for skill in skills {
        command.push_str(" skills/");
        command.push_str(skill);
    }
    command.push_str(" --method git");
    if let Some(destination) = destination {
        command.push_str(" --dest ");
        command.push_str(&shell_quote(destination));
    }
    command
}

fn assert_completed(
    result: &ExecCommandOutput,
    expected_exit: i32,
    output_fragment: &str,
) -> anyhow::Result<()> {
    ensure!(
        result.exit_code == Some(expected_exit) && result.session_id.is_none(),
        "unexpected command completion: {result:?}"
    );
    ensure!(
        result.output.contains(output_fragment),
        "command output did not preserve {output_fragment:?}: {:?}",
        result.output
    );
    Ok(())
}

fn assert_output(result: &ExecCommandOutput, expected: &str) -> anyhow::Result<()> {
    ensure!(
        result.output == expected,
        "command stdout/stderr pass-through changed: expected {expected:?}, got {:?}",
        result.output
    );
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn run_git(repository: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("/usr/bin/git")
        .current_dir(repository)
        .args(args)
        .output()
        .context("run installer fixture git")?;
    ensure!(
        output.status.success(),
        "installer fixture git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn run_git_config(config: &Path, pair: &[&str; 2]) -> anyhow::Result<()> {
    let output = Command::new("/usr/bin/git")
        .args(["config", "--file"])
        .arg(config)
        .args(pair)
        .output()
        .context("configure installer fixture git rewrite")?;
    ensure!(
        output.status.success(),
        "installer fixture git config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
