method: Intel(R) Core(TM) i7-14650HX; 6.6.87.2-microsoft-standard-WSL2; lcms2 2.17-1ubuntu0.2; rustc 1.95.0 (59807616e 2026-04-14); taskset -c 4 nice -n -5; 21 interleaved rounds; 4096x4096; binary 2026-11-19 11:47:05; load 1.50 2.58 3.19
case srgb-p3-u8: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 310.2 | 342.7 | 54.1 | 49.0 | 1.000-1.000 | 5ebff70d82fea758 |
| rusty_icc(null) | 321.0 | 340.5 | 52.3 | 49.3 | 1.000-1.000 | 5ebff70d82fea758 |
| lcms2 flags=0x0 | 110.2 | 121.0 | 152.3 | 138.6 | 1.000-1.000 | e4415c5e582b500a |
| lcms2 flags=0x440 | 112.5 | 118.3 | 149.1 | 141.8 | 1.000-1.000 | e4415c5e582b500a |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 12/21 | +0.65 | 1.003 | 1.086 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.353 | 0.329 |
| rusty_icc vs lcms2 flags=0x440 | 0/21 | -4.58 | 0.353 | 0.315 |

case srgb-p3-u16: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 362.4 | 407.0 | 46.3 | 41.2 | 1.000-1.000 | 142bb0a6f39a874e |
| rusty_icc(null) | 367.7 | 403.3 | 45.6 | 41.6 | 1.000-1.000 | 142bb0a6f39a874e |
| lcms2 flags=0x0 | 371.2 | 394.1 | 45.2 | 42.6 | 1.000-1.000 | fc251b783e904cc7 |
| lcms2 flags=0x440 | 349.8 | 378.4 | 48.0 | 44.3 | 1.000-1.000 | bf12bff2e152d1c8 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 10/21 | -0.22 | 0.998 | 1.287 |
| rusty_icc vs lcms2 flags=0x0 | 12/21 | +0.65 | 1.025 | 0.678 |
| rusty_icc vs lcms2 flags=0x440 | 6/21 | -1.96 | 0.950 | 0.621 |

case srgb-fogra-u8: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 919.8 | 994.7 | 18.2 | 16.9 | 1.000-1.000 | 855cbe7543449b42 |
| rusty_icc(null) | 902.2 | 985.9 | 18.6 | 17.0 | 1.000-1.000 | 855cbe7543449b42 |
| lcms2 flags=0x0 | 378.6 | 409.3 | 44.3 | 41.0 | 1.000-1.000 | cbaa7122cbda6182 |
| lcms2 flags=0x440 | 383.3 | 399.2 | 43.8 | 42.0 | 1.000-1.000 | 1ffa359cfa7f4009 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 12/21 | +0.65 | 1.007 | 1.083 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.417 | 0.369 |
| rusty_icc vs lcms2 flags=0x440 | 0/21 | -4.58 | 0.398 | 0.355 |

case fogra-srgb-u16: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 1319.3 | 1397.4 | 12.7 | 12.0 | 1.000-1.000 | f44efe86089793fb |
| rusty_icc(null) | 1286.4 | 1418.0 | 13.0 | 11.8 | 1.000-1.000 | f44efe86089793fb |
| lcms2 flags=0x0 | 654.4 | 704.4 | 25.6 | 23.8 | 1.000-1.000 | 9ffb1705c74b56ff |
| lcms2 flags=0x440 | 609.4 | 660.2 | 27.5 | 25.4 | 1.000-1.000 | 4d7d74db4b1f8c12 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 10/21 | -0.22 | 0.982 | 1.133 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.501 | 0.417 |
| rusty_icc vs lcms2 flags=0x440 | 0/21 | -4.58 | 0.477 | 0.406 |

case srgb-lab-f32: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 3128.2 | 3301.4 | 5.4 | 5.1 | 1.000-1.000 | 8b19459b6248fa14 |
| rusty_icc(null) | 3141.2 | 3311.9 | 5.3 | 5.1 | 1.000-1.000 | 8b19459b6248fa14 |
| lcms2 flags=0x0 | 2291.2 | 2492.5 | 7.3 | 6.7 | 1.000-1.000 | e569ffa5843a6154 |
| lcms2 flags=0x40 | 2238.6 | 2516.1 | 7.5 | 6.7 | 1.000-1.000 | e569ffa5843a6154 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 11/21 | +0.22 | 1.004 | 1.112 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.763 | 0.691 |
| rusty_icc vs lcms2 flags=0x40 | 0/21 | -4.58 | 0.758 | 0.653 |

case lab-srgb-f32: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 2205.2 | 2471.5 | 7.6 | 6.8 | 1.000-1.000 | 0002a204836f0fab |
| rusty_icc(null) | 2199.8 | 2423.8 | 7.6 | 6.9 | 1.000-1.000 | 0002a204836f0fab |
| lcms2 flags=0x0 | 1866.4 | 2023.2 | 9.0 | 8.3 | 1.000-1.000 | a1553a87074d2b1b |
| lcms2 flags=0x40 | 1898.8 | 2021.2 | 8.8 | 8.3 | 1.000-1.000 | a1553a87074d2b1b |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 12/21 | +0.65 | 1.009 | 0.910 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.831 | 0.760 |
| rusty_icc vs lcms2 flags=0x40 | 0/21 | -4.58 | 0.837 | 0.749 |

