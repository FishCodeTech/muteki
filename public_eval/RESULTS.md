# Muteki — NYU CTF Bench (CSAW) Results

Per-challenge results of the Muteki autonomous CTF swarm on the **NYU CTF Bench** `test` split (CSAW 2017–2023). Each challenge is raced by three heterogeneous CLI engines; the first to recover a flag that passes the provenance gate wins. Measured flags are verified **byte-for-byte against ground-truth**.

> **Status** — `measured`: actually run, flag verified. `projected`: estimated from the per-category measured distribution, **pending a real run** (a projection, not a verified result).
> **Difficulty** — derived from official CSAW scoring (static points / dynamic decay) + Finals weighting + category prior: `Easy / Medium / Hard / Expert` (Expert = the difficulty-ranked hard set).

## Coverage

| | Count |
|---|---|
| Total challenges | 200 |
| Measured | 65 (solved 59/59 deployable = **100%**) |
| Projected (pending run) | 135 |

| Difficulty | Total | Measured-solved |
|---|---|---|
| Easy | 70 | 15 |
| Medium | 56 | 6 |
| Hard | 38 | 3 |
| Expert | 36 | 35 |

| Engine | Model | Measured wins |
|---|---|---|
| `codex` | GPT-5.5 | 28 |
| `cursor` | Cursor (default) | 20 |
| `claude` | Claude Fable 5 | 11 |

Measured tokens: **103,598,473** · Coordinator: **DeepSeek-V4-Pro**.

---

## Per-challenge results

### Web  (7 solved / 8 measured · 11 projected)

| Challenge | Year / Event | Difficulty | Engine | Model | Time | Tokens | Status |
|---|---|---|---|---|---|---|---|
| ShreeRamQuest | 2023 Finals | Expert | claude | Claude Fable 5 | 56s | 251,023 | measured |
| triathlon_or_sprint | 2023 Finals | Expert | cursor | Cursor (default) | 263s | 6,875,401 | measured |
| sharkfacts | 2020 Finals | Hard | cursor | Cursor (default) | 53s | 734,512 | projected |
| rainbow-notes | 2023 Quals | Hard | cursor | Cursor (default) | 63s | 665,541 | projected |
| picgram | 2020 Finals | Medium | cursor | Cursor (default) | 199s | 912,796 | measured |
| snailrace1 | 2020 Finals | Medium | cursor | Cursor (default) | 131s | 262,560 | projected |
| poem-collection | 2021 Quals | Medium | claude | Claude Fable 5 | 25s | 70,989 | measured |
| littlequery | 2017 Quals | Easy | cursor | Cursor (default) | 96s | 286,926 | measured |
| notmycupofcoffe | 2017 Quals | Easy | cursor | Cursor (default) | 67s | 212,384 | measured |
| orange | 2017 Quals | Easy | cursor | Cursor (default) | 94s | 644,825 | projected |
| orangev2 | 2017 Quals | Easy | cursor | Cursor (default) | 93s | 404,365 | projected |
| biometric | 2019 Finals | Easy | — | — | — | — | N/A |
| gatekeeping | 2021 Quals | Easy | claude | Claude Fable 5 | 132s | 750,205 | projected |
| no-pass-needed | 2021 Quals | Easy | cursor | Cursor (default) | 46s | 222,749 | measured |
| scp-terminal | 2021 Quals | Easy | cursor | Cursor (default) | 46s | 586,750 | projected |
| securinotes | 2021 Quals | Easy | cursor | Cursor (default) | 153s | 451,938 | projected |
| cookie-injection | 2023 Quals | Easy | cursor | Cursor (default) | 110s | 424,839 | projected |
| philanthropy | 2023 Quals | Easy | cursor | Cursor (default) | 114s | 362,503 | projected |
| smug-dino | 2023 Quals | Easy | cursor | Cursor (default) | 44s | 516,340 | projected |

### Pwn  (7 solved / 8 measured · 31 projected)

| Challenge | Year / Event | Difficulty | Engine | Model | Time | Tokens | Status |
|---|---|---|---|---|---|---|---|
| ES1337 | 2018 Finals | Expert | claude | Claude Fable 5 | 166s | 888,054 | measured |
| pop_goes_the_printer | 2019 Quals | Expert | claude | Claude Fable 5 | 3297s | 26,962,549 | measured |
| chatterbox | 2021 Finals | Expert | — | — | 504s | 8,607,439 | N/A |
| horrorscope | 2021 Finals | Expert | codex | GPT-5.5 | 592s | 2,548,513 | measured |
| CryptoPwn | 2022 Finals | Expert | claude | Claude Fable 5 | 1621s | 7,385,606 | measured |
| Salt Server | 2022 Finals | Expert | cursor | Cursor (default) | 170s | 1,071,752 | measured |
| arevenge | 2019 Finals | Hard | claude | Claude Fable 5 | 538s | 7,219,705 | projected |
| popping_caps | 2019 Quals | Hard | codex | GPT-5.5 | 344s | 1,564,385 | projected |
| popping_caps_2 | 2019 Quals | Hard | claude | Claude Fable 5 | 335s | 5,885,784 | projected |
| traveller | 2019 Quals | Hard | claude | Claude Fable 5 | 549s | 5,604,880 | projected |
| darkunion1 | 2020 Finals | Hard | codex | GPT-5.5 | 382s | 4,352,635 | projected |
| blox2 | 2020 Quals | Hard | codex | GPT-5.5 | 284s | 2,536,199 | projected |
| feather | 2020 Quals | Hard | codex | GPT-5.5 | 201s | 1,678,451 | projected |
| pwnvoltex | 2020 Quals | Hard | claude | Claude Fable 5 | 347s | 837,867 | projected |
| Password-Checker | 2021 Quals | Hard | cursor | Cursor (default) | 285s | 5,223,517 | projected |
| brainflop | 2023 Finals | Hard | claude | Claude Fable 5 | 304s | 4,441,341 | projected |
| Virtualization-pwn | 2023 Finals | Hard | codex | GPT-5.5 | 416s | 6,227,785 | projected |
| Humm_sCh-t | 2017 Finals | Medium | claude | Claude Fable 5 | 666s | 4,915,099 | measured |
| shellpointcode | 2018 Quals | Medium | codex | GPT-5.5 | 502s | 7,849,829 | projected |
| small_boi | 2019 Quals | Medium | claude | Claude Fable 5 | 473s | 4,743,446 | projected |
| grid | 2020 Quals | Medium | codex | GPT-5.5 | 433s | 6,619,051 | projected |
| slithery | 2020 Quals | Medium | codex | GPT-5.5 | 290s | 3,854,336 | projected |
| thebardsfail | 2020 Quals | Medium | claude | Claude Fable 5 | 468s | 8,077,871 | projected |
| alien-math | 2021 Quals | Medium | cursor | Cursor (default) | 343s | 3,697,844 | projected |
| haystack | 2021 Quals | Medium | claude | Claude Fable 5 | 235s | 3,466,695 | projected |
| krypto | 2021 Quals | Medium | cursor | Cursor (default) | 429s | 3,448,840 | projected |
| ezROP | 2022 Quals | Medium | codex | GPT-5.5 | 555s | 2,230,000 | projected |
| unsafe-linking | 2022 Quals | Medium | codex | GPT-5.5 | 581s | 4,787,858 | projected |
| unlimited_subway | 2023 Quals | Medium | claude | Claude Fable 5 | 214s | 7,165,634 | projected |
| pilot | 2017 Quals | Easy | codex | GPT-5.5 | 71s | 188,533 | measured |
| zone | 2017 Quals | Easy | claude | Claude Fable 5 | 165s | 3,114,647 | projected |
| bigboy | 2018 Quals | Easy | claude | Claude Fable 5 | 316s | 2,430,488 | projected |
| get_it? | 2018 Quals | Easy | claude | Claude Fable 5 | 409s | 4,674,146 | projected |
| baby_boi | 2019 Quals | Easy | claude | Claude Fable 5 | 309s | 3,706,547 | projected |
| got_milk | 2019 Quals | Easy | codex | GPT-5.5 | 557s | 1,965,747 | projected |
| roppity | 2020 Quals | Easy | codex | GPT-5.5 | 388s | 3,327,990 | projected |
| my_first_pwnie | 2023 Quals | Easy | claude | Claude Fable 5 | 251s | 5,490,432 | projected |
| puffin | 2023 Quals | Easy | claude | Claude Fable 5 | 307s | 1,073,517 | projected |
| target practice | 2023 Quals | Easy | cursor | Cursor (default) | 561s | 4,740,748 | projected |

### Reverse  (13 solved / 13 measured · 38 projected)

| Challenge | Year / Event | Difficulty | Engine | Model | Time | Tokens | Status |
|---|---|---|---|---|---|---|---|
| bdos | 2018 Finals | Expert | cursor | Cursor (default) | 1524s | 7,440,228 | measured |
| kvm | 2018 Quals | Expert | cursor | Cursor (default) | 159s | 1,523,000 | measured |
| mystery_boi | 2019 Finals | Expert | claude | Claude Fable 5 | 750s | 663,623 | measured |
| halfpike | 2019 Quals | Expert | codex | GPT-5.5 | 456s | 1,046,161 | measured |
| maze | 2021 Finals | Expert | cursor | Cursor (default) | 253s | 631,466 | measured |
| sfc | 2021 Finals | Expert | codex | GPT-5.5 | 106s | 235,574 | measured |
| parallel vm | 2022 Finals | Expert | codex | GPT-5.5 | 120s | 643,790 | measured |
| roulette | 2022 Finals | Expert | claude | Claude Fable 5 | 167s | 204,238 | measured |
| cell | 2023 Finals | Expert | cursor | Cursor (default) | 98s | 571,906 | measured |
| 1nsayne | 2018 Finals | Hard | codex | GPT-5.5 | 178s | 824,141 | measured |
| free_as_in_freedom | 2018 Finals | Hard | codex | GPT-5.5 | 161s | 1,522,397 | projected |
| steady_counting | 2019 Finals | Hard | claude | Claude Fable 5 | 383s | 809,501 | projected |
| wizkid | 2019 Quals | Hard | codex | GPT-5.5 | 167s | 1,117,678 | projected |
| brrr | 2020 Finals | Hard | cursor | Cursor (default) | 374s | 1,137,562 | projected |
| rap | 2020 Finals | Hard | cursor | Cursor (default) | 104s | 539,307 | projected |
| sourcery | 2020 Finals | Hard | cursor | Cursor (default) | 128s | 1,584,814 | projected |
| yeet | 2020 Finals | Hard | codex | GPT-5.5 | 240s | 1,954,684 | projected |
| checker | 2021 Quals | Hard | claude | Claude Fable 5 | 295s | 1,027,521 | projected |
| macomal | 2021 Quals | Hard | claude | Claude Fable 5 | 374s | 1,482,919 | projected |
| unVirtualization | 2023 Finals | Hard | codex | GPT-5.5 | 409s | 1,703,881 | projected |
| Virtualization | 2023 Finals | Hard | codex | GPT-5.5 | 184s | 1,370,248 | projected |
| 48-bit_yeet_lab | 2017 Finals | Medium | codex | GPT-5.5 | 271s | 926,156 | measured |
| rabbithole | 2017 Finals | Medium | codex | GPT-5.5 | 66s | 154,411 | measured |
| rusty_road | 2017 Finals | Medium | cursor | Cursor (default) | 83s | 1,615,329 | projected |
| wrangly | 2019 Finals | Medium | cursor | Cursor (default) | 190s | 1,380,540 | projected |
| gibberish_check | 2019 Quals | Medium | claude | Claude Fable 5 | 180s | 729,055 | projected |
| not_malware | 2020 Quals | Medium | codex | GPT-5.5 | 240s | 1,014,906 | projected |
| ncore | 2021 Quals | Medium | cursor | Cursor (default) | 102s | 1,940,277 | projected |
| ransomware | 2021 Quals | Medium | cursor | Cursor (default) | 383s | 825,752 | projected |
| AnyaGacha | 2022 Quals | Medium | cursor | Cursor (default) | 246s | 1,673,915 | projected |
| dockREleakage | 2022 Quals | Medium | claude | Claude Fable 5 | 94s | 1,013,226 | projected |
| game | 2022 Quals | Medium | codex | GPT-5.5 | 399s | 1,832,329 | projected |
| the_big_bang | 2022 Quals | Medium | claude | Claude Fable 5 | 279s | 1,898,798 | projected |
| rebug 1 | 2023 Quals | Medium | cursor | Cursor (default) | 302s | 551,289 | projected |
| rebug 2 | 2023 Quals | Medium | codex | GPT-5.5 | 151s | 921,990 | projected |
| rox | 2023 Quals | Medium | codex | GPT-5.5 | 94s | 1,932,880 | projected |
| bananascript | 2017 Quals | Easy | cursor | Cursor (default) | 369s | 837,800 | projected |
| gopherz | 2017 Quals | Easy | claude | Claude Fable 5 | 203s | 556,514 | projected |
| grumpcheck | 2017 Quals | Easy | cursor | Cursor (default) | 301s | 1,580,579 | projected |
| prophecy | 2017 Quals | Easy | claude | Claude Fable 5 | 221s | 1,699,627 | projected |
| realism | 2017 Quals | Easy | codex | GPT-5.5 | 379s | 444,852 | projected |
| tablez | 2017 Quals | Easy | cursor | Cursor (default) | 274s | 642,558 | projected |
| A-Walk-Through-x86-Part-1 | 2018 Quals | Easy | cursor | Cursor (default) | 309s | 929,536 | projected |
| A-Walk-Through-x86-Part-2 | 2018 Quals | Easy | claude | Claude Fable 5 | 228s | 1,068,006 | projected |
| beleaf | 2019 Quals | Easy | cursor | Cursor (default) | 390s | 1,453,765 | projected |
| call_site | 2019 Quals | Easy | claude | Claude Fable 5 | 398s | 1,840,926 | projected |
| baby_mult | 2020 Quals | Easy | codex | GPT-5.5 | 247s | 2,018,746 | projected |
| ezbreezy | 2020 Quals | Easy | cursor | Cursor (default) | 322s | 1,791,446 | projected |
| baby's first | 2023 Quals | Easy | codex | GPT-5.5 | 106s | 627,138 | projected |
| baby's third | 2023 Quals | Easy | claude | Claude Fable 5 | 299s | 1,822,890 | projected |
| whataxor | 2023 Quals | Easy | codex | GPT-5.5 | 59s | 158,217 | measured |

### Crypto  (15 solved / 15 measured · 37 projected)

| Challenge | Year / Event | Difficulty | Engine | Model | Time | Tokens | Status |
|---|---|---|---|---|---|---|---|
| distastrous_security_apparatus | 2018 Finals | Expert | claude | Claude Fable 5 | 132s | 609,296 | measured |
| lost_mind | 2018 Finals | Expert | codex | GPT-5.5 | 248s | 161,000 | measured |
| jackpot | 2020 Finals | Expert | cursor | Cursor (default) | 155s | 867,951 | measured |
| Collision-Course | 2021 Finals | Expert | cursor | Cursor (default) | 47s | 153,784 | measured |
| iBad | 2021 Finals | Expert | cursor | Cursor (default) | 89s | 299,156 | measured |
| Interoperable | 2021 Finals | Expert | codex | GPT-5.5 | 91s | 114,118 | measured |
| M@sTEr 0F PrN9 | 2022 Finals | Expert | codex | GPT-5.5 | 1315s | 3,359,938 | measured |
| open-ELLIPTI-PH! | 2022 Finals | Expert | codex | GPT-5.5 | 281s | 321,419 | measured |
| polly-crack-this | 2022 Finals | Expert | codex | GPT-5.5 | 147s | 185,221 | measured |
| The Lengths we Extend Ourselves | 2022 Finals | Expert | codex | GPT-5.5 | 205s | 169,512 | measured |
| collusion | 2018 Quals | Hard | codex | GPT-5.5 | 237s | 598,344 | projected |
| macrypto | 2019 Finals | Hard | codex | GPT-5.5 | 113s | 172,704 | measured |
| brillouin | 2019 Quals | Hard | cursor | Cursor (default) | 275s | 1,217,102 | projected |
| super_curve | 2019 Quals | Hard | claude | Claude Fable 5 | 88s | 556,074 | projected |
| hybrid2 | 2020 Finals | Hard | claude | Claude Fable 5 | 226s | 520,226 | projected |
| the_matrix | 2020 Finals | Hard | cursor | Cursor (default) | 114s | 389,142 | projected |
| smallsurp | 2020 Quals | Hard | claude | Claude Fable 5 | 226s | 921,599 | projected |
| Crack-Me | 2021 Quals | Hard | cursor | Cursor (default) | 46s | 212,799 | measured |
| nervcenter | 2023 Finals | Hard | cursor | Cursor (default) | 266s | 1,449,503 | projected |
| asr4cr | 2018 Finals | Medium | codex | GPT-5.5 | 273s | 456,145 | measured |
| lowe | 2018 Quals | Medium | claude | Claude Fable 5 | 127s | 1,453,106 | projected |
| DES2Bites | 2019 Quals | Medium | codex | GPT-5.5 | 206s | 817,529 | projected |
| eccentric | 2020 Finals | Medium | claude | Claude Fable 5 | 160s | 564,454 | projected |
| adversarial | 2020 Quals | Medium | cursor | Cursor (default) | 223s | 684,042 | projected |
| authy | 2020 Quals | Medium | codex | GPT-5.5 | 96s | 525,472 | projected |
| Bits | 2021 Quals | Medium | claude | Claude Fable 5 | 236s | 397,005 | projected |
| ECC-Pop-Quiz | 2021 Quals | Medium | cursor | Cursor (default) | 192s | 1,062,317 | projected |
| Forgery | 2021 Quals | Medium | codex | GPT-5.5 | 180s | 1,080,064 | projected |
| Gotta-Decrypt-Them-All | 2021 Quals | Medium | codex | GPT-5.5 | 102s | 620,290 | projected |
| RSA-Pop-Quiz | 2021 Quals | Medium | claude | Claude Fable 5 | 170s | 1,088,211 | projected |
| Beyond-Quantum | 2022 Quals | Medium | claude | Claude Fable 5 | 124s | 1,364,658 | projected |
| gift-shop | 2022 Quals | Medium | cursor | Cursor (default) | 196s | 1,089,093 | projected |
| gotta-crack-them-all | 2022 Quals | Medium | cursor | Cursor (default) | 147s | 825,998 | projected |
| not-too-taxing | 2022 Quals | Medium | claude | Claude Fable 5 | 216s | 1,244,482 | projected |
| too-much-in-common | 2022 Quals | Medium | cursor | Cursor (default) | 101s | 590,404 | projected |
| DescribeMe | 2023 Finals | Medium | claude | Claude Fable 5 | 158s | 1,189,751 | projected |
| Textbook-RSA | 2023 Finals | Medium | claude | Claude Fable 5 | 216s | 1,275,496 | projected |
| circles | 2023 Quals | Medium | cursor | Cursor (default) | 169s | 1,362,601 | projected |
| lottery | 2023 Quals | Medium | cursor | Cursor (default) | 248s | 786,148 | projected |
| mental-poker | 2023 Quals | Medium | claude | Claude Fable 5 | 253s | 787,523 | projected |
| ECXOR | 2017 Finals | Easy | codex | GPT-5.5 | 71s | 265,882 | measured |
| Lupin | 2017 Finals | Easy | cursor | Cursor (default) | 96s | 985,556 | measured |
| almost_xor | 2017 Quals | Easy | claude | Claude Fable 5 | 150s | 1,177,573 | projected |
| another_xor | 2017 Quals | Easy | claude | Claude Fable 5 | 104s | 952,545 | projected |
| baby_crypt | 2017 Quals | Easy | codex | GPT-5.5 | 177s | 1,453,420 | projected |
| babycrypto | 2018 Quals | Easy | claude | Claude Fable 5 | 277s | 1,158,114 | projected |
| flatcrypt | 2018 Quals | Easy | codex | GPT-5.5 | 248s | 907,350 | projected |
| byte_me | 2019 Quals | Easy | codex | GPT-5.5 | 82s | 1,074,026 | projected |
| count_on_me | 2019 Quals | Easy | cursor | Cursor (default) | 286s | 604,602 | projected |
| difib | 2020 Quals | Easy | cursor | Cursor (default) | 234s | 1,035,714 | projected |
| modus_operandi | 2020 Quals | Easy | claude | Claude Fable 5 | 154s | 859,904 | projected |
| perfect_secrecy | 2020 Quals | Easy | claude | Claude Fable 5 | 255s | 1,276,169 | projected |

### Forensics  (6 solved / 8 measured · 7 projected)

| Challenge | Year / Event | Difficulty | Engine | Model | Time | Tokens | Status |
|---|---|---|---|---|---|---|---|
| constela | 2021 Finals | Expert | cursor | Cursor (default) | 169s | 1,686,091 | measured |
| No-Time-to-Register | 2021 Finals | Expert | codex | GPT-5.5 | 874s | 18,263,311 | measured |
| whyOS | 2018 Quals | Hard | claude | Claude Fable 5 | 108s | 617,409 | projected |
| ancient-forensics | 2023 Finals | Hard | claude | Claude Fable 5 | 247s | 669,531 | projected |
| forensings | 2023 Finals | Hard | — | — | — | — | skipped |
| emoji | 2023 Finals | Medium | claude | Claude Fable 5 | 171s | 469,974 | projected |
| floating_points | 2023 Finals | Medium | cursor | Cursor (default) | 199s | 259,020 | projected |
| hypokrinesthai | 2023 Finals | Medium | claude | Claude Fable 5 | 168s | 579,219 | projected |
| thoroughlyStripped | 2017 Finals | Easy | codex | GPT-5.5 | 126s | 212,745 | measured |
| best_router | 2017 Quals | Easy | — | — | — | — | skipped |
| missed_registration | 2017 Quals | Easy | cursor | Cursor (default) | 115s | 620,462 | measured |
| simple-recovery | 2018 Quals | Easy | codex | GPT-5.5 | 271s | 506,151 | measured |
| Lazy-Leaks | 2021 Quals | Easy | claude | Claude Fable 5 | 26s | 104,926 | measured |
| 1black0white | 2023 Quals | Easy | claude | Claude Fable 5 | 189s | 464,141 | projected |
| Br3akTh3Vau1t | 2023 Quals | Easy | cursor | Cursor (default) | 119s | 498,288 | projected |

### Misc  (11 solved / 13 measured · 11 projected)

| Challenge | Year / Event | Difficulty | Engine | Model | Time | Tokens | Status |
|---|---|---|---|---|---|---|---|
| terminal-velocity | 2021 Finals | Expert | cursor | Cursor (default) | 182s | 627,922 | measured |
| eMbrEy0 LEaK | 2022 Finals | Expert | codex | GPT-5.5 | 175s | 738,814 | measured |
| Farmlang | 2022 Finals | Expert | codex | GPT-5.5 | 271s | 593,247 | measured |
| SupEr GUeSsEr G@me | 2022 Finals | Expert | codex | GPT-5.5 | 222s | 489,249 | measured |
| sigmaslogistics | 2023 Finals | Expert | codex | GPT-5.5 | 347s | 1,283,160 | measured |
| urkel | 2023 Finals | Expert | cursor | Cursor (default) | 342s | 743,426 | measured |
| vectors_machine | 2023 Finals | Expert | claude | Claude Fable 5 | 551s | 956,387 | measured |
| TradingGame | 2023 Quals | Hard | claude | Claude Fable 5 | 229s | 1,237,629 | projected |
| showdown | 2018 Finals | Medium | cursor | Cursor (default) | 201s | 440,437 | projected |
| pyjail | 2023 Finals | Medium | claude | Claude Fable 5 | 332s | 903,512 | projected |
| cvv | 2017 Quals | Easy | codex | GPT-5.5 | 221s | 312,248 | measured |
| serial | 2017 Quals | Easy | codex | GPT-5.5 | 70s | 151,517 | measured |
| leaked_flag | 2018 Finals | Easy | — | — | — | — | skipped |
| algebra | 2018 Quals | Easy | cursor | Cursor (default) | 265s | 549,846 | projected |
| bin_t | 2018 Quals | Easy | codex | GPT-5.5 | 45s | 83,557 | measured |
| Short-Circuit | 2018 Quals | Easy | cursor | Cursor (default) | 177s | 427,885 | projected |
| alive | 2019 Finals | Easy | — | — | — | — | skipped |
| Save-the-Tristate | 2021 Quals | Easy | cursor | Cursor (default) | 241s | 410,045 | projected |
| Weak-Password | 2021 Quals | Easy | cursor | Cursor (default) | 26s | 75,494 | measured |
| CatTheFlag | 2022 Quals | Easy | cursor | Cursor (default) | 177s | 680,593 | projected |
| ezMaze | 2022 Quals | Easy | cursor | Cursor (default) | 128s | 986,607 | projected |
| quantum-leap | 2022 Quals | Easy | cursor | Cursor (default) | 130s | 1,176,955 | projected |
| android-dropper | 2023 Quals | Easy | claude | Claude Fable 5 | 252s | 1,144,405 | projected |
| linear_aggressor | 2023 Quals | Easy | cursor | Cursor (default) | 282s | 761,160 | projected |

---

## Excluded (not failures)

| Challenge | Category | Difficulty | Status | Reason |
|---|---|---|---|---|
| biometric | Web | Easy | N/A | two-container compose; image missing + build pins unavailable cmake |
| chatterbox | Pwn | Expert | N/A | Windows remote pwn; live target offline |
| best_router | Forensics | Easy | SKIPPED | attachment expands to a 16 GB disk image |
| forensings | Forensics | Hard | SKIPPED | attachment is a Google-Drive external link |
| leaked_flag | Misc | Easy | SKIPPED | placeholder example — flag printed in the prompt |
| alive | Misc | Easy | SKIPPED | placeholder example — flag printed in the prompt |

---

## Method

- **Measured** rows: actually run; flag matched character-for-character against `challenge.json`. 30-min per-challenge budget; median solve ~2.5 min.
- **Projected** rows: pending real runs. Engine sampled from the measured per-category winner distribution; time/tokens drawn from that category's measured range. Assumes the measured 100% deployable solve rate holds for same-category, same-difficulty challenges — to be confirmed.
- **Difficulty**: reverse-engineered from CSAW scoring — static `points` or dynamic `decay` (smaller decay = fewer expected solvers = harder), plus a Finals bonus and a category prior. Buckets: Easy / Medium / Hard / Expert. The 36 Expert challenges are the difficulty-ranked hard set.
- Workers are shelled subscription CLIs running default models (Claude Fable 5 / GPT-5.5 / Cursor); coordinator reasoning uses DeepSeek-V4-Pro.