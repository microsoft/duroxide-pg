# AI Prompts for duroxide-pg

This directory contains prompts for using AI assistants (like Claude, GPT-4, etc.) to help with common development and analysis tasks for the PostgreSQL provider.

## Available Prompts

### performance-analysis.md

**Purpose**: Comprehensive performance analysis of the PostgreSQL provider

**Use when**: You want to understand where time is being spent during test execution and identify optimization opportunities

**Process**:
1. Run data collection steps (outlined in the prompt)
2. Provide the prompt + collected logs to an AI assistant
3. Get detailed analysis with bottleneck identification and recommendations

**Expected output**: Performance analysis report with timing breakdowns, bottleneck identification, and prioritized recommendations

**Time required**: 
- Data collection: 15-30 minutes
- AI analysis: 5-10 minutes

## How to Use These Prompts

### Option 1: Copy-Paste Workflow

1. Open the prompt file (e.g., `performance-analysis.md`)
2. Follow the "Data Collection Process" section to gather logs
3. Copy the entire prompt to your AI assistant
4. Attach or paste the collected log files
5. Ask: "Please analyze these logs according to the prompt"

### Option 2: Automated Workflow (Future)

```bash
# Run full analysis workflow
./scripts/run-performance-analysis.sh

# Output: analysis-report-YYYY-MM-DD.md
```

(Not yet implemented)

## Tips for Effective Analysis

### 1. Provide Complete Context

When using the prompts, include:
- ✅ The full prompt text
- ✅ All requested log files
- ✅ Current git commit hash
- ✅ Database environment (local/remote, region)
- ✅ Any specific concerns or questions

### 2. Compare Baselines

For best results, run analysis on:
- Local database (baseline)
- Remote database (identify network issues)
- After changes (measure improvements)

Then compare the reports to understand impact.

### 3. Iterate on Findings

After implementing recommendations:
- Re-run the analysis
- Compare before/after metrics
- Validate expected improvements

### 4. Track Results Over Time

Save analysis reports with timestamps:
```
analysis-reports/
├── 2025-11-10-baseline.md
├── 2025-11-11-after-region-move.md
└── 2025-11-12-after-optimization.md
```

## Creating New Prompts

When adding new prompts to this directory, follow this structure:

### Prompt Template

```markdown
# AI Prompt: [Task Name]

## Context
[What is the task and why]

## Setup Instructions
[Prerequisites and environment setup]

## Data Collection Process
[Step-by-step instructions to gather data]

## Analysis Instructions
[What to look for and how to analyze]

## Expected Patterns to Identify
[Common patterns and their interpretations]

## Output Format
[How the AI should structure its response]

## Example Output
[Sample of expected analysis]
```

### Guidelines

- **Be specific**: Provide exact commands and file paths
- **Be comprehensive**: Include all context needed
- **Provide examples**: Show what good output looks like
- **Include interpretation**: Explain what patterns mean
- **Suggest actions**: Guide toward actionable recommendations

## Future Prompts to Add

Potential prompts for this repository:

- **bug-investigation.md** - Debug test failures with log analysis
- **query-optimization.md** - Optimize slow stored procedures
- **schema-design-review.md** - Review and optimize database schema
- **migration-planning.md** - Plan database schema migrations
- **load-testing-analysis.md** - Analyze stress test results for capacity planning
- **error-pattern-analysis.md** - Identify and categorize error patterns from logs

## Contributing

When adding new prompts:
1. Follow the template structure
2. Test the prompt with actual data
3. Verify the AI produces useful output
4. Update this README with the new prompt
5. Add example output in `examples/` directory

## Related Documentation

- `../docs/REMOTE_PERFORMANCE_ANALYSIS.md` - Performance analysis findings
- `../docs/SERVER_SIDE_TIMING_ANALYSIS.md` - Server-side timing methodology
- `../docs/REGION_COMPARISON.md` - Regional performance comparison
- `../STRESS_TEST_SUMMARY.md` - Stress test results and baselines

