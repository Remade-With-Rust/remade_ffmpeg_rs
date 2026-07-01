#!/bin/bash
# Quality gate: corpus mean PEAQ ODG (+ NMR % audible) for the current ./ffmpeg encoder.
LABEL="$1"
S="/c/Users/talmo/AppData/Local/Temp/claude/c--Users-talmo-coding-remade-ffmpeg-rs/d3234c79-a622-4891-b393-dfed899bb5dc/scratchpad"
SYS="/c/Users/talmo/AppData/Local/Microsoft/WinGet/Packages/Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe/ffmpeg-8.1.2-full_build/bin/ffmpeg.exe"
OURS="./target/release/ffmpeg.exe"; Q="./target/release/examples/mp3quality"
CLIPS="Ring05 Ring09 Ring01 chimes"; BRS="64 128"
for c in $CLIPS; do [ -f "$S/corp_$c.wav" ] || "$SYS" -hide_banner -loglevel error -t 4 -i "/c/Windows/Media/$c.wav" -ac 1 -ar 44100 -c:a pcm_f32le -y "$S/corp_$c.wav"; done
echo "clip,br,odg,pct" > "$S/eval_$LABEL.csv"
for c in $CLIPS; do for br in $BRS; do
  "$OURS" -i "$S/corp_$c.wav" -c:a mp3 -b:a ${br}k -y "$S/e.mp3" >/dev/null 2>&1
  "$SYS" -hide_banner -loglevel error -i "$S/e.mp3" -ac 1 -ar 44100 -c:a pcm_s16le -y "$S/e.wav"
  odg=$(python "$S/peaq_run.py" "$S/corp_$c.wav" "$S/e.wav" "$S/PEAQ_python" 2>/dev/null | awk '{print $2}')
  pc=$("$Q" "$S/corp_$c.wav" "c=$S/e.wav" 2>/dev/null | awk '/^c /{print $6}')
  echo "$c,$br,$odg,$pc" >> "$S/eval_$LABEL.csv"
done; done
python -c "
import csv,statistics as st
r=[x for x in csv.DictReader(open(r'C:/Users/talmo/AppData/Local/Temp/claude/c--Users-talmo-coding-remade-ffmpeg-rs/d3234c79-a622-4891-b393-dfed899bb5dc/scratchpad/eval_$LABEL.csv')) if x['odg']]
odg=[float(x['odg']) for x in r]
print('[$LABEL] mean ODG = %.3f  (n=%d)  worst clip ODG = %.3f'%(st.mean(odg),len(odg),min(odg)))
for x in r: print('   %-8s %4s  ODG %+6.3f  %%aud %s'%(x['clip'],x['br'],float(x['odg']),x['pct']))
"
