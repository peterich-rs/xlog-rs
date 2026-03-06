# 20260306 sync buffer perf

| scenario | backend | throughput_mps | lat_avg_ns | lat_p99_ns | vs previous rust baseline | rust/cpp |
| :--- | :--- | ---: | ---: | ---: | :--- | ---: |
| sync1t | rust | 646165.728 | 1398.495 | 5111.000 | throughput +193.5% | 1.119 |
| sync1t | cpp | 577243.262 | 1598.283 | 5861.333 | n/a | 1.000 |
| sync4t | rust | 327021.122 | 11893.722 | 83319.667 | throughput +95.4% | 0.870 |
| sync4t | cpp | 375983.162 | 10359.508 | 52555.667 | n/a | 1.000 |
