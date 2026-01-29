pub mod agent;
pub mod cache;
pub mod hash;
pub mod judge;
pub mod parse;
pub mod report;

pub use agent::{invoke_agent, AgentResult};
pub use cache::{ensure_report_dir, should_skip_cached};
pub use hash::git_hash_file;
pub use judge::{invoke_judge, parse_judge_output, JudgeRating};
pub use parse::{parse_test_file, substitute_variables, TestCase, Timespan};
pub use report::{generate_summary, FailureRecord, FailureStage};
