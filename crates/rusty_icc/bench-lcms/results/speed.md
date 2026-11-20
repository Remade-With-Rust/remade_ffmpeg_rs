method: Intel(R) Core(TM) i7-14650HX; 6.6.87.2-microsoft-standard-WSL2; lcms2 2.17-1ubuntu0.2; rustc 1.95.0 (59807616e 2026-04-14); taskset -c 4 nice -n -5; 21 interleaved rounds; 4096x4096; binary 2026-11-19 19:39:31; load 3.16 3.74 2.67
case srgb-p3-u8: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 243.1 | 252.7 | 69.0 | 66.4 | 1.000-1.000 | 5ebff70d82fea758 |
| rusty_icc(null) | 244.8 | 252.6 | 68.5 | 66.4 | 1.000-1.000 | 5ebff70d82fea758 |
| lcms2 flags=0x0 | 93.2 | 98.2 | 180.1 | 170.8 | 1.000-1.000 | e4415c5e582b500a |
| lcms2 flags=0x440 | 92.7 | 97.3 | 181.0 | 172.4 | 1.000-1.000 | e4415c5e582b500a |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 10/21 | -0.22 | 0.999 | 1.093 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.383 | 0.361 |
| rusty_icc vs lcms2 flags=0x440 | 0/21 | -4.58 | 0.389 | 0.368 |

case srgb-p3-u16: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 261.9 | 272.2 | 64.1 | 61.6 | 1.000-1.000 | 142bb0a6f39a874e |
| rusty_icc(null) | 259.6 | 274.3 | 64.6 | 61.2 | 1.000-1.000 | 142bb0a6f39a874e |
| lcms2 flags=0x0 | 303.0 | 311.7 | 55.4 | 53.8 | 1.000-1.000 | fc251b783e904cc7 |
| lcms2 flags=0x440 | 286.4 | 295.9 | 58.6 | 56.7 | 1.000-1.000 | bf12bff2e152d1c8 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 11/21 | +0.22 | 1.005 | 0.924 |
| rusty_icc vs lcms2 flags=0x0 | 21/21 | +4.58 | 1.152 | 1.222 |
| rusty_icc vs lcms2 flags=0x440 | 20/21 | +4.15 | 1.090 | 1.171 |

case srgb-fogra-u8: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 468.6 | 483.2 | 35.8 | 34.7 | 0.999-1.000 | 855cbe7543449b42 |
| rusty_icc(null) | 461.4 | 483.2 | 36.4 | 34.7 | 1.000-1.000 | 855cbe7543449b42 |
| lcms2 flags=0x0 | 323.3 | 338.6 | 51.9 | 49.5 | 0.999-1.000 | cbaa7122cbda6182 |
| lcms2 flags=0x440 | 309.6 | 320.9 | 54.2 | 52.3 | 1.000-1.000 | 1ffa359cfa7f4009 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 9/21 | -0.65 | 0.998 | 1.082 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.698 | 0.664 |
| rusty_icc vs lcms2 flags=0x440 | 0/21 | -4.58 | 0.664 | 0.638 |

case fogra-srgb-u16: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 810.6 | 850.4 | 20.7 | 19.7 | 1.000-1.000 | f44efe86089793fb |
| rusty_icc(null) | 819.8 | 845.7 | 20.5 | 19.8 | 1.000-1.000 | f44efe86089793fb |
| lcms2 flags=0x0 | 538.5 | 560.6 | 31.2 | 29.9 | 1.000-1.000 | 9ffb1705c74b56ff |
| lcms2 flags=0x440 | 515.2 | 526.2 | 32.6 | 31.9 | 1.000-1.000 | 4d7d74db4b1f8c12 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 12/21 | +0.65 | 1.005 | 0.946 |
| rusty_icc vs lcms2 flags=0x0 | 0/21 | -4.58 | 0.666 | 0.629 |
| rusty_icc vs lcms2 flags=0x440 | 1/21 | -4.15 | 0.622 | 0.597 |

case srgb-lab-f32: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 2131.2 | 2201.8 | 7.9 | 7.6 | 1.000-1.000 | 8b19459b6248fa14 |
| rusty_icc(null) | 2131.6 | 2197.2 | 7.9 | 7.6 | 1.000-1.000 | 8b19459b6248fa14 |
| lcms2 flags=0x0 | 1963.3 | 2025.4 | 8.5 | 8.3 | 1.000-1.000 | e569ffa5843a6154 |
| lcms2 flags=0x40 | 1966.7 | 2047.9 | 8.5 | 8.2 | 1.000-1.000 | e569ffa5843a6154 |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 10/21 | -0.22 | 0.999 | 1.280 |
| rusty_icc vs lcms2 flags=0x0 | 3/21 | -3.27 | 0.929 | 1.118 |
| rusty_icc vs lcms2 flags=0x40 | 3/21 | -3.27 | 0.939 | 1.204 |

case lab-srgb-f32: 4096x4096 = 16.78 Mpx, 21 pairs, intent relative, single thread
| arm | min ms | median ms | Mpx/s (min) | Mpx/s (median) | cpu/wall range | output FNV |
|---|---:|---:|---:|---:|---|---|
| rusty_icc | 1473.4 | 1499.8 | 11.4 | 11.2 | 1.000-1.000 | 0002a204836f0fab |
| rusty_icc(null) | 1449.7 | 1501.5 | 11.6 | 11.2 | 1.000-1.000 | 0002a204836f0fab |
| lcms2 flags=0x0 | 1680.5 | 1721.2 | 10.0 | 9.7 | 1.000-1.000 | a1553a87074d2b1b |
| lcms2 flags=0x40 | 1677.6 | 1729.3 | 10.0 | 9.7 | 1.000-1.000 | a1553a87074d2b1b |

| pair | rusty_icc faster in | z | median ratio (other / rusty_icc) | worst-pair ratio |
|---|---:|---:|---:|---:|
| rusty_icc vs rusty_icc(null) | 9/21 | -0.65 | 0.999 | 1.030 |
| rusty_icc vs lcms2 flags=0x0 | 21/21 | +4.58 | 1.143 | 1.186 |
| rusty_icc vs lcms2 flags=0x40 | 21/21 | +4.58 | 1.147 | 1.236 |

