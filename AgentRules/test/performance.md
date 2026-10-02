---
load-when: writing or changing performance tests or benchmarks
applies-to: every repository
title: Performance tests
---

# Performance Tests

Arrange the system state, dataset, and environment; act by running the workload under test;
measure timing, throughput, memory, or resource usage; assert the result against the threshold.

- Measure one characteristic at a time, clearly.
- Set the acceptance threshold before running the test.
- Compare against a known baseline, using repeat runs to smooth out noise.
- Use a stable environment; a result that varies with the machine is not a result.
- Keep performance tests out of the fast unit suite.
- Record what the test measures and why it matters.
