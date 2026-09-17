# Phase 0 Estimator Feasibility Report

## Falsification rule

> If `candidate` does not beat `taskclass_repo` on `regret_usd` at matched admit-rate, the hypothesis is not supported by this evidence. Coverage/pinball/sharpness numbers are supporting evidence, not the headline finding.

## Headline finding

**Verdict: MIXED** -- candidate beat taskclass_repo on regret_usd in 8/14 matched (split_config, budget) comparisons.

| split_config | budget_usd | candidate regret_usd | taskclass_repo regret_usd | candidate wins |
|---|---|---|---|---|
| cold_start | 0.5 | 1257.12 | 2310.00 | yes |
| cold_start | 1.0 | 2076.51 | 3315.00 | yes |
| cold_start | 2.0 | 1805.10 | 3965.00 | yes |
| cold_start | 3.0 | 1854.29 | 4270.00 | yes |
| cold_start | 5.0 | 0.57 | 0.57 | no |
| cold_start | 8.0 | 0.00 | 0.00 | no |
| cold_start | 12.0 | 0.00 | 0.00 | no |
| history_assisted | 0.5 | 1592.53 | 2310.00 | yes |
| history_assisted | 1.0 | 2076.58 | 3315.00 | yes |
| history_assisted | 2.0 | 1876.68 | 3965.00 | yes |
| history_assisted | 3.0 | 1867.34 | 4149.61 | yes |
| history_assisted | 5.0 | 0.57 | 0.57 | no |
| history_assisted | 8.0 | 0.00 | 0.00 | no |
| history_assisted | 12.0 | 0.00 | 0.00 | no |

## Dataset

- n_rows: 2904
- n_instances: 500
- n_submissions: 6
- censoring_rate: 0.32920110192837465
- resolved_rate: 0.3205922865013774
- n_uncensored_rows: 1948
- escalation_triggered: True

> **ESCALATION**: uncensored row count is below the configured min_uncensored_rows threshold. Per HORO-1120, this is a dataset-adequacy judgment for HORO-1123 to decide, not something this harness routes around.

## Supporting evidence: coverage / pinball / sharpness

(Supporting evidence only -- see falsification rule above.)

| split_config | model_name | target | quantile | empirical_coverage | pinball_loss | n | oracle_probe | status |
|---|---|---|---|---|---|---|---|---|
| cold_start | global_median | instance_cost_usd | 0.5 | 0.488 | 0.6679 | 2904 | False | ok |
| cold_start | global_median | instance_cost_usd | 0.8 | 0.793 | 0.4455 | 2904 | False | ok |
| cold_start | global_median | instance_cost_usd | 0.9 | 0.898 | 0.2275 | 2904 | False | ok |
| cold_start | taskclass_repo | instance_cost_usd | 0.5 | 0.488 | 0.6679 | 2904 | False | ok |
| cold_start | taskclass_repo | instance_cost_usd | 0.8 | 0.793 | 0.4455 | 2904 | False | ok |
| cold_start | taskclass_repo | instance_cost_usd | 0.9 | 0.898 | 0.2275 | 2904 | False | ok |
| cold_start | taskclass_difficulty | instance_cost_usd | 0.5 | 0.493 | 0.6587 | 2904 | True | ok |
| cold_start | taskclass_difficulty | instance_cost_usd | 0.8 | 0.794 | 0.4495 | 2904 | True | ok |
| cold_start | taskclass_difficulty | instance_cost_usd | 0.9 | 0.895 | 0.2276 | 2904 | True | ok |
| cold_start | llm_self_estimate | instance_cost_usd | 0.5 | n/a | n/a | 2904 | False | unavailable |
| cold_start | llm_self_estimate | instance_cost_usd | 0.8 | n/a | n/a | 2904 | False | unavailable |
| cold_start | llm_self_estimate | instance_cost_usd | 0.9 | n/a | n/a | 2904 | False | unavailable |
| cold_start | knn_history | instance_cost_usd | 0.5 | 0.505 | 0.5175 | 2904 | False | ok |
| cold_start | knn_history | instance_cost_usd | 0.8 | 0.710 | 0.3620 | 2904 | False | ok |
| cold_start | knn_history | instance_cost_usd | 0.9 | 0.758 | 0.2464 | 2904 | False | ok |
| cold_start | candidate | instance_cost_usd | 0.5 | 0.478 | 0.4997 | 2904 | False | ok |
| cold_start | candidate | instance_cost_usd | 0.8 | 0.770 | 0.3488 | 2904 | False | ok |
| cold_start | candidate | instance_cost_usd | 0.9 | 0.902 | 0.1697 | 2904 | False | ok |
| history_assisted | global_median | instance_cost_usd | 0.5 | 0.499 | 0.6644 | 2904 | False | ok |
| history_assisted | global_median | instance_cost_usd | 0.8 | 0.798 | 0.4454 | 2904 | False | ok |
| history_assisted | global_median | instance_cost_usd | 0.9 | 0.900 | 0.2275 | 2904 | False | ok |
| history_assisted | taskclass_repo | instance_cost_usd | 0.5 | 0.501 | 0.6587 | 2904 | False | ok |
| history_assisted | taskclass_repo | instance_cost_usd | 0.8 | 0.799 | 0.4503 | 2904 | False | ok |
| history_assisted | taskclass_repo | instance_cost_usd | 0.9 | 0.895 | 0.2277 | 2904 | False | ok |
| history_assisted | taskclass_difficulty | instance_cost_usd | 0.5 | 0.502 | 0.6553 | 2904 | True | ok |
| history_assisted | taskclass_difficulty | instance_cost_usd | 0.8 | 0.800 | 0.4455 | 2904 | True | ok |
| history_assisted | taskclass_difficulty | instance_cost_usd | 0.9 | 0.899 | 0.2276 | 2904 | True | ok |
| history_assisted | llm_self_estimate | instance_cost_usd | 0.5 | n/a | n/a | 2904 | False | unavailable |
| history_assisted | llm_self_estimate | instance_cost_usd | 0.8 | n/a | n/a | 2904 | False | unavailable |
| history_assisted | llm_self_estimate | instance_cost_usd | 0.9 | n/a | n/a | 2904 | False | unavailable |
| history_assisted | knn_history | instance_cost_usd | 0.5 | 0.503 | 0.5120 | 2904 | False | ok |
| history_assisted | knn_history | instance_cost_usd | 0.8 | 0.707 | 0.3551 | 2904 | False | ok |
| history_assisted | knn_history | instance_cost_usd | 0.9 | 0.763 | 0.2373 | 2904 | False | ok |
| history_assisted | candidate | instance_cost_usd | 0.5 | 0.487 | 0.4614 | 2904 | False | ok |
| history_assisted | candidate | instance_cost_usd | 0.8 | 0.800 | 0.2945 | 2904 | False | ok |
| history_assisted | candidate | instance_cost_usd | 0.9 | 0.912 | 0.1670 | 2904 | False | ok |
| cold_start | global_median | wall_clock_seconds | 0.5 | 0.483 | 196.9570 | 2904 | False | ok |
| cold_start | global_median | wall_clock_seconds | 0.8 | 0.784 | 201.6581 | 2904 | False | ok |
| cold_start | global_median | wall_clock_seconds | 0.9 | 0.888 | 136.1418 | 2904 | False | ok |
| cold_start | taskclass_repo | wall_clock_seconds | 0.5 | 0.483 | 196.9570 | 2904 | False | ok |
| cold_start | taskclass_repo | wall_clock_seconds | 0.8 | 0.784 | 201.6581 | 2904 | False | ok |
| cold_start | taskclass_repo | wall_clock_seconds | 0.9 | 0.888 | 136.1418 | 2904 | False | ok |
| cold_start | taskclass_difficulty | wall_clock_seconds | 0.5 | 0.481 | 194.8730 | 2904 | True | ok |
| cold_start | taskclass_difficulty | wall_clock_seconds | 0.8 | 0.781 | 195.0977 | 2904 | True | ok |
| cold_start | taskclass_difficulty | wall_clock_seconds | 0.9 | 0.886 | 135.5948 | 2904 | True | ok |
| cold_start | llm_self_estimate | wall_clock_seconds | 0.5 | n/a | n/a | 2904 | False | unavailable |
| cold_start | llm_self_estimate | wall_clock_seconds | 0.8 | n/a | n/a | 2904 | False | unavailable |
| cold_start | llm_self_estimate | wall_clock_seconds | 0.9 | n/a | n/a | 2904 | False | unavailable |
| cold_start | knn_history | wall_clock_seconds | 0.5 | 0.512 | 215.7730 | 2904 | False | ok |
| cold_start | knn_history | wall_clock_seconds | 0.8 | 0.697 | 186.3549 | 2904 | False | ok |
| cold_start | knn_history | wall_clock_seconds | 0.9 | 0.752 | 140.4044 | 2904 | False | ok |
| cold_start | candidate | wall_clock_seconds | 0.5 | 0.486 | 201.5603 | 2904 | False | ok |
| cold_start | candidate | wall_clock_seconds | 0.8 | 0.784 | 183.6174 | 2904 | False | ok |
| cold_start | candidate | wall_clock_seconds | 0.9 | 0.898 | 105.5783 | 2904 | False | ok |
| history_assisted | global_median | wall_clock_seconds | 0.5 | 0.500 | 195.4081 | 2904 | False | ok |
| history_assisted | global_median | wall_clock_seconds | 0.8 | 0.799 | 196.8172 | 2904 | False | ok |
| history_assisted | global_median | wall_clock_seconds | 0.9 | 0.898 | 135.7226 | 2904 | False | ok |
| history_assisted | taskclass_repo | wall_clock_seconds | 0.5 | 0.500 | 193.9917 | 2904 | False | ok |
| history_assisted | taskclass_repo | wall_clock_seconds | 0.8 | 0.794 | 193.2532 | 2904 | False | ok |
| history_assisted | taskclass_repo | wall_clock_seconds | 0.9 | 0.895 | 136.9908 | 2904 | False | ok |
| history_assisted | taskclass_difficulty | wall_clock_seconds | 0.5 | 0.499 | 193.6337 | 2904 | True | ok |
| history_assisted | taskclass_difficulty | wall_clock_seconds | 0.8 | 0.798 | 192.5287 | 2904 | True | ok |
| history_assisted | taskclass_difficulty | wall_clock_seconds | 0.9 | 0.899 | 134.6477 | 2904 | True | ok |
| history_assisted | llm_self_estimate | wall_clock_seconds | 0.5 | n/a | n/a | 2904 | False | unavailable |
| history_assisted | llm_self_estimate | wall_clock_seconds | 0.8 | n/a | n/a | 2904 | False | unavailable |
| history_assisted | llm_self_estimate | wall_clock_seconds | 0.9 | n/a | n/a | 2904 | False | unavailable |
| history_assisted | knn_history | wall_clock_seconds | 0.5 | 0.508 | 209.0439 | 2904 | False | ok |
| history_assisted | knn_history | wall_clock_seconds | 0.8 | 0.707 | 180.9466 | 2904 | False | ok |
| history_assisted | knn_history | wall_clock_seconds | 0.9 | 0.767 | 134.2122 | 2904 | False | ok |
| history_assisted | candidate | wall_clock_seconds | 0.5 | 0.473 | 184.9953 | 2904 | False | ok |
| history_assisted | candidate | wall_clock_seconds | 0.8 | 0.807 | 153.4504 | 2904 | False | ok |
| history_assisted | candidate | wall_clock_seconds | 0.9 | 0.895 | 102.4501 | 2904 | False | ok |

## Candidate n_estimators choice (per split_config / target / fold)

| split_config | target | chosen_n_estimators (by quantile) |
|---|---|---|
| cold_start | instance_cost_usd | {'0.5': 50, '0.8': 50, '0.9': 50} |
| cold_start | instance_cost_usd | {'0.5': 200, '0.8': 100, '0.9': 200} |
| cold_start | instance_cost_usd | {'0.5': 100, '0.8': 200, '0.9': 200} |
| cold_start | instance_cost_usd | {'0.5': 100, '0.8': 50, '0.9': 50} |
| cold_start | instance_cost_usd | {'0.5': 100, '0.8': 50, '0.9': 50} |
| history_assisted | instance_cost_usd | {'0.5': 200, '0.8': 50, '0.9': 200} |
| history_assisted | instance_cost_usd | {'0.5': 200, '0.8': 50, '0.9': 50} |
| history_assisted | instance_cost_usd | {'0.5': 200, '0.8': 50, '0.9': 100} |
| history_assisted | instance_cost_usd | {'0.5': 200, '0.8': 50, '0.9': 50} |
| history_assisted | instance_cost_usd | {'0.5': 50, '0.8': 100, '0.9': 200} |
| cold_start | wall_clock_seconds | {'0.5': 50, '0.8': 50, '0.9': 50} |
| cold_start | wall_clock_seconds | {'0.5': 50, '0.8': 100, '0.9': 50} |
| cold_start | wall_clock_seconds | {'0.5': 50, '0.8': 50, '0.9': 50} |
| cold_start | wall_clock_seconds | {'0.5': 100, '0.8': 50, '0.9': 50} |
| cold_start | wall_clock_seconds | {'0.5': 50, '0.8': 50, '0.9': 50} |
| history_assisted | wall_clock_seconds | {'0.5': 50, '0.8': 100, '0.9': 50} |
| history_assisted | wall_clock_seconds | {'0.5': 100, '0.8': 50, '0.9': 50} |
| history_assisted | wall_clock_seconds | {'0.5': 50, '0.8': 50, '0.9': 200} |
| history_assisted | wall_clock_seconds | {'0.5': 50, '0.8': 50, '0.9': 50} |
| history_assisted | wall_clock_seconds | {'0.5': 50, '0.8': 50, '0.9': 100} |

## Run metadata

- seed: 1120
- git_sha: 79b1a177a47c25b10c7a54ce3a1fc29606abd10d
- config_sha256: 2bb84821db901067fb51d74f0c8297c6bec974dfbfe0fa7544252ec66e5c83ec
- dataset_fingerprint: sha256:e05ad330f481e1ff6cc1259d62ca60c1f828e44f4057b1cc6c58966bc1d38c49
- generated_at: 2026-09-17T05:16:20.885952+00:00

## Known limitations

See PROVENANCE.md for the full list (sample size, harness diversity, proxy time target, censoring bias, Python-only generalization).
