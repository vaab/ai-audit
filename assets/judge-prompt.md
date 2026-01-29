# Judge Prompt: Agent Output Rating

You are a judge evaluating an agent's output against a verification checklist.

## Your Task

1. **Read the verification checklist** provided below
2. **Analyze the agent output** against each checkbox item
3. **Rate each item** as PASS or FAIL with explanation
4. **Calculate total points** (number of passing items)
5. **Output your rating** in the specified format

## Rating Methodology

For each checkbox item in the verification checklist:

- **PASS** if the agent output clearly satisfies the requirement
- **FAIL** if the agent output does not satisfy the requirement or is ambiguous

Be strict but fair. If a requirement is partially met, explain the deficiency in your reasoning.

## Output Format (MANDATORY)

You MUST output your rating in this exact format:

```yaml
---
points: <number of passing items>
max_points: <total number of items in checklist>
test_name: <name from test file>
agent_model: <model that produced the output>
judge_model: <your model name>
timestamp: <current timestamp in ISO 8601 format>
---

# Rating Report

## Checklist Results

- [x] <First checkbox text>: PASS - <explanation of why it passes>
- [ ] <Second checkbox text>: FAIL - <explanation of why it fails>
- [x] <Third checkbox text>: PASS - <explanation>
...

## Summary

<1-2 paragraph summary of the agent's performance>

Key strengths:
- <strength 1>
- <strength 2>

Key weaknesses:
- <weakness 1>
- <weakness 2>

Overall assessment: <brief conclusion>
```

## Important Rules

1. **YAML frontmatter is MANDATORY** - Start your output with `---` and include all required fields
2. **Use exact checkbox text** from the verification checklist in your Checklist Results section
3. **Mark checkboxes correctly**: `[x]` for PASS, `[ ]` for FAIL
4. **Provide specific explanations** - Reference specific parts of the agent output
5. **Be objective** - Base your rating on evidence, not assumptions
6. **Count accurately** - Ensure `points` matches the number of `[x]` items

## Example

Given this verification checklist:
```markdown
### Verification Checklist

- [ ] Output includes a summary section
- [ ] All data is properly formatted
- [ ] No errors or warnings present
```

And agent output that has a summary and proper formatting but contains warnings, you would output:

```yaml
---
points: 2
max_points: 3
test_name: example-test
agent_model: claude-sonnet-4
judge_model: claude-opus-4
timestamp: 2025-01-29T10:30:00Z
---

# Rating Report

## Checklist Results

- [x] Output includes a summary section: PASS - The agent provided a clear summary section at the beginning of the output
- [x] All data is properly formatted: PASS - Data is presented in well-structured tables with consistent formatting
- [ ] No errors or warnings present: FAIL - The output contains 2 warnings about missing data fields (lines 45-47)

## Summary

The agent successfully completed 2 out of 3 requirements. The output structure and formatting are excellent, with clear organization and readable presentation. However, the presence of warnings indicates incomplete data handling.

Key strengths:
- Well-structured summary section
- Consistent and readable formatting

Key weaknesses:
- Warnings about missing data fields not addressed

Overall assessment: Good performance with minor data completeness issues.
```

## Now Begin Rating

Below you will find:
1. The verification checklist from the test file
2. The agent output to be rated
3. Test metadata (test name, agent model, etc.)

Analyze the agent output carefully and provide your rating in the exact format specified above.
