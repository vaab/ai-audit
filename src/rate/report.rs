//! Summary report generation for AI rating results.
//!
//! Aggregates judge ratings across multiple agents and judges
//! to produce a markdown summary report.

use std::collections::HashMap;
use std::fmt;

use chrono::{DateTime, Utc};

use crate::rate::judge::JudgeRating;

/// Stage at which a failure occurred during the rating process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureStage {
    /// Failed to parse test file
    TestParsing,
    /// Failed to invoke the agent
    AgentInvocation,
    /// Agent timed out
    AgentTimeout,
    /// Failed to invoke the judge
    JudgeInvocation,
    /// Judge timed out
    JudgeTimeout,
    /// Failed to parse judge output
    JudgeParsing,
    /// Failed to write report file
    ReportWriting,
}

impl fmt::Display for FailureStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailureStage::TestParsing => write!(f, "Test Parsing"),
            FailureStage::AgentInvocation => write!(f, "Agent Invocation"),
            FailureStage::AgentTimeout => write!(f, "Agent Timeout"),
            FailureStage::JudgeInvocation => write!(f, "Judge Invocation"),
            FailureStage::JudgeTimeout => write!(f, "Judge Timeout"),
            FailureStage::JudgeParsing => write!(f, "Judge Parsing"),
            FailureStage::ReportWriting => write!(f, "Report Writing"),
        }
    }
}

/// Record of a failure that occurred during the rating process.
#[derive(Debug, Clone)]
pub struct FailureRecord {
    /// Stage at which the failure occurred
    pub stage: FailureStage,
    /// Model involved (agent or judge, if applicable)
    pub model: Option<String>,
    /// Error message
    pub message: String,
    /// When the failure occurred
    pub timestamp: DateTime<Utc>,
}

/// Per-agent statistics aggregated across judges.
#[derive(Debug)]
struct AgentStats {
    /// Ratings from each judge: judge_model -> (points, max_points)
    judge_ratings: HashMap<String, (u32, u32)>,
    /// Total points across all judges
    total_points: u32,
    /// Total max points across all judges
    total_max_points: u32,
}

impl AgentStats {
    fn new() -> Self {
        Self {
            judge_ratings: HashMap::new(),
            total_points: 0,
            total_max_points: 0,
        }
    }

    fn add_rating(&mut self, judge_model: &str, points: u32, max_points: u32) {
        self.judge_ratings
            .insert(judge_model.to_string(), (points, max_points));
        self.total_points += points;
        self.total_max_points += max_points;
    }

    fn average_percentage(&self) -> f64 {
        if self.total_max_points == 0 {
            0.0
        } else {
            (self.total_points as f64 / self.total_max_points as f64) * 100.0
        }
    }

    fn average_points(&self) -> f64 {
        if self.judge_ratings.is_empty() {
            0.0
        } else {
            self.total_points as f64 / self.judge_ratings.len() as f64
        }
    }

    fn max_points_per_judge(&self) -> u32 {
        self.judge_ratings
            .values()
            .map(|(_, max)| *max)
            .next()
            .unwrap_or(0)
    }
}

/// Generate a summary report from judge ratings and failure records.
///
/// # Arguments
/// * `ratings` - All successful judge ratings
/// * `errors` - All failures that occurred during the process
/// * `test_name` - Name of the test being summarized
/// * `agent_instruction_hash` - Hash of agent instruction file
/// * `judge_prompt_hash` - Hash of judge prompt file
///
/// # Returns
/// Markdown-formatted summary report
pub fn generate_summary(
    ratings: &[JudgeRating],
    errors: &[FailureRecord],
    test_name: &str,
    agent_instruction_hash: &str,
    judge_prompt_hash: &str,
) -> String {
    let timestamp = Utc::now();
    let mut output = String::new();

    // Header
    output.push_str("# Rating Summary\n\n");
    output.push_str(&format!("**Test**: {}\n", test_name));
    output.push_str(&format!(
        "**Agent Instruction Hash**: {}\n",
        agent_instruction_hash
    ));
    output.push_str(&format!("**Judge Prompt Hash**: {}\n", judge_prompt_hash));
    output.push_str(&format!(
        "**Timestamp**: {}\n",
        timestamp.format("%Y-%m-%dT%H:%M:%SZ")
    ));

    // Handle empty ratings
    if ratings.is_empty() {
        output.push_str("\n## No Ratings\n\n");
        output.push_str("No successful ratings were collected.\n");

        // Still include errors if any
        if !errors.is_empty() {
            output.push_str(&format_errors_section(errors));
        }

        return output;
    }

    // Group ratings by agent
    let mut agent_stats: HashMap<String, AgentStats> = HashMap::new();
    for rating in ratings {
        let stats = agent_stats
            .entry(rating.agent_model.clone())
            .or_insert_with(AgentStats::new);
        stats.add_rating(&rating.judge_model, rating.points, rating.max_points);
    }

    // Per-Agent Results
    output.push_str("\n## Per-Agent Results\n");

    // Sort agents alphabetically for consistent output
    let mut agent_names: Vec<_> = agent_stats.keys().collect();
    agent_names.sort();

    for agent_model in agent_names {
        let stats = &agent_stats[agent_model];
        output.push_str(&format!("\n### {}\n", agent_model));

        // Sort judge ratings alphabetically
        let mut judge_names: Vec<_> = stats.judge_ratings.keys().collect();
        judge_names.sort();

        for judge_model in judge_names {
            let (points, max_points) = stats.judge_ratings[judge_model];
            let pct = if max_points > 0 {
                (points as f64 / max_points as f64) * 100.0
            } else {
                0.0
            };
            output.push_str(&format!(
                "- {}: {}/{} ({:.1}%)\n",
                judge_model, points, max_points, pct
            ));
        }

        // Agent average
        let avg_points = stats.average_points();
        let max_per_judge = stats.max_points_per_judge();
        let avg_pct = stats.average_percentage();
        output.push_str(&format!(
            "- **Average**: {:.1}/{} ({:.1}%)\n",
            avg_points, max_per_judge, avg_pct
        ));
    }

    // Global Average
    output.push_str("\n## Global Average\n\n");
    let total_points: u32 = ratings.iter().map(|r| r.points).sum();
    let total_max_points: u32 = ratings.iter().map(|r| r.max_points).sum();
    let global_avg_points = if ratings.is_empty() {
        0.0
    } else {
        total_points as f64 / ratings.len() as f64
    };
    let max_per_rating = ratings.first().map(|r| r.max_points).unwrap_or(0);
    let global_pct = if total_max_points > 0 {
        (total_points as f64 / total_max_points as f64) * 100.0
    } else {
        0.0
    };
    output.push_str(&format!(
        "**All Judges × All Agents**: {:.2}/{} ({:.1}%)\n",
        global_avg_points, max_per_rating, global_pct
    ));

    // Errors section
    if !errors.is_empty() {
        output.push_str(&format_errors_section(errors));
    }

    output
}

/// Format the errors section of the report.
fn format_errors_section(errors: &[FailureRecord]) -> String {
    let mut output = String::new();
    output.push_str("\n## Errors\n\n");

    for error in errors {
        let model_info = match &error.model {
            Some(model) => format!("{}: {} - ", error.stage, model),
            None => format!("{} - ", error.stage),
        };
        output.push_str(&format!("- {}{}\n", model_info, error.message));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rating(
        agent_model: &str,
        judge_model: &str,
        points: u32,
        max_points: u32,
    ) -> JudgeRating {
        JudgeRating {
            points,
            max_points,
            test_name: "test".to_string(),
            agent_model: agent_model.to_string(),
            judge_model: judge_model.to_string(),
            timestamp: Utc::now(),
            full_report: "report".to_string(),
        }
    }

    #[test]
    fn test_generate_summary_single_agent() {
        let ratings = vec![
            make_rating("claude-sonnet-4", "claude-opus-4", 8, 11),
            make_rating("claude-sonnet-4", "gpt-4o", 9, 11),
        ];

        let summary = generate_summary(&ratings, &[], "work-deamalgamate", "abc123", "def456");

        // Check header
        assert!(summary.contains("# Rating Summary"));
        assert!(summary.contains("**Test**: work-deamalgamate"));
        assert!(summary.contains("**Agent Instruction Hash**: abc123"));
        assert!(summary.contains("**Judge Prompt Hash**: def456"));

        // Check agent section
        assert!(summary.contains("### claude-sonnet-4"));
        assert!(summary.contains("claude-opus-4: 8/11"));
        assert!(summary.contains("gpt-4o: 9/11"));

        // Check average calculation: (8+9)/2 = 8.5 out of 11
        assert!(summary.contains("**Average**: 8.5/11"));

        // Global average: 8.5/11 = 77.3%
        assert!(summary.contains("**All Judges × All Agents**"));
    }

    #[test]
    fn test_generate_summary_multiple_agents() {
        let ratings = vec![
            make_rating("claude-sonnet-4", "claude-opus-4", 8, 11),
            make_rating("claude-sonnet-4", "gpt-4o", 9, 11),
            make_rating("gpt-4", "claude-opus-4", 7, 11),
            make_rating("gpt-4", "gpt-4o", 7, 11),
        ];

        let summary = generate_summary(&ratings, &[], "work-deamalgamate", "abc123", "def456");

        // Check both agents present
        assert!(summary.contains("### claude-sonnet-4"));
        assert!(summary.contains("### gpt-4"));

        // Check gpt-4 ratings
        assert!(summary.contains("gpt-4"));

        // Global average section
        assert!(summary.contains("## Global Average"));
    }

    #[test]
    fn test_generate_summary_with_errors() {
        let ratings = vec![make_rating("claude-sonnet-4", "claude-opus-4", 8, 11)];

        let errors = vec![
            FailureRecord {
                stage: FailureStage::AgentTimeout,
                model: Some("gpt-4-turbo".to_string()),
                message: "Timeout after 300s".to_string(),
                timestamp: Utc::now(),
            },
            FailureRecord {
                stage: FailureStage::JudgeParsing,
                model: Some("claude-opus-4".to_string()),
                message: "Parse error: missing YAML frontmatter".to_string(),
                timestamp: Utc::now(),
            },
        ];

        let summary = generate_summary(&ratings, &errors, "work-deamalgamate", "abc123", "def456");

        // Check errors section
        assert!(summary.contains("## Errors"));
        assert!(summary.contains("Agent Timeout: gpt-4-turbo - Timeout after 300s"));
        assert!(summary
            .contains("Judge Parsing: claude-opus-4 - Parse error: missing YAML frontmatter"));
    }

    #[test]
    fn test_generate_summary_empty_ratings() {
        let errors = vec![FailureRecord {
            stage: FailureStage::TestParsing,
            model: None,
            message: "Invalid test file".to_string(),
            timestamp: Utc::now(),
        }];

        let summary = generate_summary(&[], &errors, "broken-test", "abc", "def");

        assert!(summary.contains("## No Ratings"));
        assert!(summary.contains("No successful ratings were collected"));
        assert!(summary.contains("## Errors"));
        assert!(summary.contains("Test Parsing - Invalid test file"));
    }

    #[test]
    fn test_generate_summary_no_errors() {
        let ratings = vec![make_rating("agent", "judge", 5, 10)];

        let summary = generate_summary(&ratings, &[], "test", "hash1", "hash2");

        // Should not contain errors section
        assert!(!summary.contains("## Errors"));
    }

    #[test]
    fn test_failure_stage_display() {
        assert_eq!(format!("{}", FailureStage::TestParsing), "Test Parsing");
        assert_eq!(
            format!("{}", FailureStage::AgentInvocation),
            "Agent Invocation"
        );
        assert_eq!(format!("{}", FailureStage::AgentTimeout), "Agent Timeout");
        assert_eq!(
            format!("{}", FailureStage::JudgeInvocation),
            "Judge Invocation"
        );
        assert_eq!(format!("{}", FailureStage::JudgeTimeout), "Judge Timeout");
        assert_eq!(format!("{}", FailureStage::JudgeParsing), "Judge Parsing");
        assert_eq!(format!("{}", FailureStage::ReportWriting), "Report Writing");
    }

    #[test]
    fn test_agent_stats_calculations() {
        let mut stats = AgentStats::new();
        stats.add_rating("judge1", 8, 10);
        stats.add_rating("judge2", 6, 10);

        // Total: 14/20 = 70%
        assert!((stats.average_percentage() - 70.0).abs() < 0.01);

        // Average points: 14/2 = 7
        assert!((stats.average_points() - 7.0).abs() < 0.01);

        // Max points per judge
        assert_eq!(stats.max_points_per_judge(), 10);
    }

    #[test]
    fn test_agent_stats_empty() {
        let stats = AgentStats::new();
        assert_eq!(stats.average_percentage(), 0.0);
        assert_eq!(stats.average_points(), 0.0);
        assert_eq!(stats.max_points_per_judge(), 0);
    }

    #[test]
    fn test_percentage_calculations() {
        let ratings = vec![
            make_rating("agent", "judge1", 8, 11),
            make_rating("agent", "judge2", 9, 11),
        ];

        let summary = generate_summary(&ratings, &[], "test", "h1", "h2");

        // 8/11 = 72.7%
        assert!(summary.contains("72.7%"));
        // 9/11 = 81.8%
        assert!(summary.contains("81.8%"));
    }

    #[test]
    fn test_perfect_score() {
        let ratings = vec![make_rating("agent", "judge", 10, 10)];

        let summary = generate_summary(&ratings, &[], "test", "h1", "h2");
        assert!(summary.contains("100.0%"));
    }

    #[test]
    fn test_zero_score() {
        let ratings = vec![make_rating("agent", "judge", 0, 10)];

        let summary = generate_summary(&ratings, &[], "test", "h1", "h2");
        assert!(summary.contains("0/10"));
        assert!(summary.contains("0.0%"));
    }
}
