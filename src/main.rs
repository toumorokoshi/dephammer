use clap::Parser;
use rkyv;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::process::Command;
use tracing_subscriber;

mod bazel;
mod git;
use log::{error, info};

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Analyzes and identifies removable Bazel dependencies"
)]
struct Args {
    /// The subcommand to run
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Analyze removable dependencies
    Analyze {
        /// The target to analyze
        target: String,

        /// Test targets to verify against
        #[arg(long, required = true)]
        test: Vec<String>,
    },
    /// Find targets that trigger core dumps
    TriggerScores {
        /// Path to the workspace root
        workspace_path: String,

        /// The target to analyze
        target: String,

        /// The maximum number of commit history to consider
        #[arg(long)]
        since: Option<String>,

        /// Path to the dependencies file
        #[arg(long)]
        deps_file: Option<String>,

        /// Path to the git analysis file
        #[arg(long)]
        git_analysis_file: Option<String>,
    },
    TriggerScoresMap {
        /// Path to the workspace root
        workspace_path: String,

        /// The target to analyze
        target: String,

        /// The maximum number of commit history to consider
        #[arg(long)]
        since: Option<String>,

        /// Path to the dependencies file
        #[arg(long)]
        deps_file: Option<String>,

        /// Path to the git analysis file
        #[arg(long)]
        git_analysis_file: Option<String>,

        /// The format to output the results in
        #[arg(long, default_value = "yaml")]
        format: String,
    },
    /// Analyze Bazel dependency graph
    AnalyzeBazelDeps {
        /// Path to the workspace root
        workspace_path: String,

        /// The target to analyze
        target: String,

        /// Path to the dependencies file
        #[arg(long)]
        output: String,
    },
    /// Analyze git repository data, outputting a JSON file
    AnalyzeGitRepo {
        /// Path to the workspace root
        workspace_path: String,

        /// Path to output JSON file
        #[arg(long)]
        output: String,

        /// The maximum number of commit history to consider
        #[arg(long)]
        since: Option<String>,
    },
}

fn main() {
    main_inner().unwrap();
}

fn main_inner() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();

    match args.command {
        Commands::Analyze { target, test } => {
            info!("Analyzing target: {}", target);
            info!("Test targets:");
            for test_target in &test {
                info!("  {}", test_target);
            }

            // Get deps for the target
            let deps = get_deps(&target);
            let mut removable_deps = Vec::new();

            // Try removing each dep
            for dep in deps {
                if test_passes_without_dep(&target, &dep, &test) {
                    removable_deps.push(dep);
                }
            }

            // Print results
            if removable_deps.is_empty() {
                println!("\nNo removable dependencies found.");
            } else {
                println!("\nThe following dependencies can potentially be removed:");
                for dep in removable_deps {
                    println!("  {}", dep);
                }
            }
            Ok(())
        }
        Commands::TriggerScores {
            workspace_path: workspace_root,
            target,
            since,
            deps_file,
            git_analysis_file,
        } => {
            let deps_graph = if let Some(deps_file) = deps_file {
                bazel::BazelDependencyGraph::from_file(&deps_file)?
            } else {
                bazel::BazelDependencyGraph::from_workspace(&workspace_root, &target)
            };

            let repo = if let Some(git_analysis_file) = git_analysis_file {
                git::GitRepo::from_file(&git_analysis_file).unwrap()
            } else {
                git::GitRepo::from_path(&workspace_root, since).unwrap()
            };

            let trigger_score = calculate_trigger_scores(&target, &repo, &deps_graph)?;
            println!("Trigger score for {}: {}", target, trigger_score);
            Ok(())
        }
        Commands::TriggerScoresMap {
            workspace_path: workspace_root,
            target,
            since,
            deps_file,
            git_analysis_file,
            format,
        } => {
            let deps_graph = if let Some(deps_file) = deps_file {
                bazel::BazelDependencyGraph::from_file(&deps_file)?
            } else {
                bazel::BazelDependencyGraph::from_workspace(&workspace_root, &target)
            };

            let repo = if let Some(git_analysis_file) = git_analysis_file {
                git::GitRepo::from_file(&git_analysis_file).unwrap()
            } else {
                git::GitRepo::from_path(&workspace_root, since).unwrap()
            };

            let scores_by_target = calculate_trigger_scores_map(&target, &repo, &deps_graph)?;
            let mut sorted_scores: Vec<_> = scores_by_target.iter().collect();
            sorted_scores.sort_by(|a, b| b.1.cmp(a.1));
            let targets = sorted_scores.iter().map(|(k, v)| (*v).clone()).collect();
            let trigger_scores = TriggerScores { targets };
            match format.as_str() {
                "yaml" => {
                    let yaml_output = serde_yaml::to_string(&trigger_scores)?;
                    println!("{}", yaml_output);
                }
                "csv" => {
                    let mut wtr = csv::Writer::from_writer(std::io::stdout());
                    // Serialize each target as a row
                    for target in &trigger_scores.targets {
                        wtr.serialize(target)?;
                    }
                    wtr.flush()?;
                }
                _ => {
                    panic!("Unsupported format: {}", format);
                }
            }
            Ok(())
        }

        Commands::AnalyzeGitRepo {
            workspace_path,
            output,
            since,
        } => {
            let repo = git::GitRepo::from_path(&workspace_path, since).unwrap();
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&repo)?;

            let mut file = File::create(output).unwrap();
            file.write_all(&bytes).unwrap();
            Ok(())
        }
        Commands::AnalyzeBazelDeps {
            workspace_path,
            target,
            output,
        } => {
            let deps_graph = bazel::BazelDependencyGraph::from_workspace(&workspace_path, &target);
            let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&deps_graph)?;
            let mut file = File::create(output).unwrap();
            file.write_all(&bytes).unwrap();
            Ok(())
        }
    }
}

fn get_deps(target: &str) -> Vec<String> {
    let cmd_args = ["print deps", target];
    info!("Executing: buildozer {}", cmd_args.join(" "));

    let output = Command::new("buildozer")
        .args(cmd_args)
        .output()
        .expect("Failed to execute buildozer");

    if !output.status.success() {
        error!(
            "buildozer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

fn remove_dep(target: &str, dep: &str) -> bool {
    let cmd = format!("remove deps {}", dep);
    info!("Executing: buildozer {} {}", cmd, target);

    let output = Command::new("buildozer")
        .args([&cmd, target])
        .output()
        .expect("Failed to execute buildozer");

    if !output.status.success() {
        error!(
            "buildozer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return false;
    }

    true
}

fn add_dep(target: &str, dep: &str) -> bool {
    let cmd = format!("add deps {}", dep);
    info!("Executing: buildozer {} {}", cmd, target);

    let output = Command::new("buildozer")
        .args([&cmd, target])
        .output()
        .expect("Failed to execute buildozer");

    if !output.status.success() {
        error!(
            "buildozer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return false;
    }

    true
}

fn test_passes_without_dep(target: &str, dep: &str, test_targets: &Vec<String>) -> bool {
    remove_dep(target, dep);
    let mut success = true;
    for test in test_targets {
        info!("executing: bazel test {}", test);

        let output = Command::new("bazel")
            .args(["test", test])
            .output()
            .expect("Failed to execute bazel");

        if !output.status.success() {
            success = false;
            error!(
                "bazel test failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    // re-add the dep at the end
    add_dep(target, dep);
    success
}

fn calculate_trigger_scores(
    target: &str,
    repo: &git::GitRepo,
    deps_graph: &bazel::BazelDependencyGraph,
) -> Result<usize, Box<dyn Error>> {
    info!("calculating trigger scores for target: {}", target);
    let source_files = deps_graph.get_source_files(target, true)?;
    info!("found {} source files", source_files.len());
    let mut all_commits: std::collections::HashSet<String> = std::collections::HashSet::new();
    for source_file in source_files {
        // we don't care about remote dependencies
        if source_file.starts_with("@") {
            continue;
        }
        let parts: Vec<&str> = source_file.split(':').collect();
        let relative_path = &format!("{}/{}", parts[0], parts[1])[2..];

        // println!("Analyzing source file: {}", source_file);
        if let Some(file) = repo.files.get(relative_path) {
            // println!("Found {} commits for {}", commits.len(), source_file);
            all_commits.extend(file.commit_history.iter().cloned());
        }
    }
    Ok(all_commits.len())
}

#[derive(Debug, Serialize, Deserialize)]
struct TriggerScores {
    targets: Vec<Target>,
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq, Clone)]
struct Target {
    name: String,
    /// number of times the target is rebuilt
    rebuilds: usize,
    /// number of targets that depend on this target
    dependents: usize,
    /// score refers to how much the target is responsible for triggering
    /// builds. it is currently rebuilds + dependents.
    score: usize,
}

impl Ord for Target {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rebuilds.cmp(&other.rebuilds)
    }
}

impl PartialOrd for Target {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.rebuilds.cmp(&other.rebuilds))
    }
}

fn calculate_trigger_scores_map(
    target: &str,
    repo: &git::GitRepo,
    deps_graph: &bazel::BazelDependencyGraph,
) -> Result<HashMap<String, Target>, Box<dyn Error>> {
    let mut commits_by_target = HashMap::new();
    let mut score_by_target = HashMap::new();
    if target.ends_with("...") {
        let prefix = target[..target.len() - 4].to_string();
        // we grab all targets from the map, in this case.
        for (t, _) in deps_graph.rules_by_label.iter() {
            if t.starts_with(&prefix) {
                calculate_trigger_scores_map_inner(
                    t,
                    repo,
                    deps_graph,
                    &mut commits_by_target,
                    &mut score_by_target,
                )?;
            }
        }
    } else {
        calculate_trigger_scores_map_inner(
            target,
            repo,
            deps_graph,
            &mut commits_by_target,
            &mut score_by_target,
        )?;
    }
    for (_, target) in score_by_target.iter_mut() {
        target.score = target.rebuilds * target.dependents;
    }
    Ok(score_by_target)
}

fn calculate_trigger_scores_map_inner(
    target: &str,
    repo: &git::GitRepo,
    deps_graph: &bazel::BazelDependencyGraph,
    commits_by_target: &mut HashMap<String, std::collections::HashSet<String>>,
    score_by_target: &mut HashMap<String, Target>,
) -> Result<std::collections::HashSet<String>, Box<dyn Error>> {
    if let Some(commits) = commits_by_target.get(target) {
        return Ok(commits.clone());
    }
    let mut all_commits: std::collections::HashSet<String> = std::collections::HashSet::new();
    let rule = deps_graph
        .rules_by_label
        .get(target)
        .ok_or(format!("target {} not found in dependency graph", target))?;
    for dep_target in rule.dep_targets.iter() {
        all_commits.extend(calculate_trigger_scores_map_inner(
            dep_target,
            repo,
            deps_graph,
            commits_by_target,
            score_by_target,
        )?);
        score_by_target
            .entry(dep_target.to_string())
            .and_modify(|t| t.dependents += 1);
    }
    for source_file in rule.source_files.iter() {
        // we don't care about remote dependencies
        if source_file.starts_with("@") {
            continue;
        }
        let parts: Vec<&str> = source_file.split(':').collect();
        let relative_path = &format!("{}/{}", parts[0], parts[1])[2..];

        // println!("Analyzing source file: {}", source_file);
        if let Some(file) = repo.files.get(relative_path) {
            // println!("Found {} commits for {}", commits.len(), source_file);
            all_commits.extend(file.commit_history.iter().cloned());
        }
    }
    score_by_target.insert(
        target.to_string(),
        Target {
            name: target.to_string(),
            rebuilds: all_commits.len(),
            dependents: 1, // dependents always includes the target itself
            score: 0,
        },
    );
    commits_by_target.insert(target.to_string(), all_commits.clone());
    Ok(all_commits)
}
