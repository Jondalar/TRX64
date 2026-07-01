# CLI-Feel — 60-Sekunden-Durchklick (manueller Test)

Fresh binary: `/usr/local/bin/trx64cli` (Symlink → `target/release/trx64cli`, von S9
neu gebaut). Start: einfach `trx64cli`. Beenden: `/quit` (oder Ctrl-C bei leerer Zeile).

Jeder Punkt = ein Vision-Ziel → Tastenfolge → erwartetes Ergebnis. `⇥` = Tab.

## A. Drei Namespaces
1. `/help` ⏎ → Hilfe zeigt drei Namespaces: `/` (Maschine), `!` (Dateisystem), bare (Monitor).
2. `d c000` ⏎ → Disassembler-Ausgabe (bare = Monitor).
3. `!pwd` ⏎ → Projekt-CWD (`!` = Dateisystem).
4. `ls` ⏎ (bare, ohne `!`) → Hinweis „filesystem commands live behind '!' — try !ls" (Nudge, führt NICHT aus).

## B. Tab-Autocomplete (alle 3 Namespaces + Pfade)
5. `/mo`⇥ → vervollständigt zu `/mount `.
6. `/`⇥ → Liste der `/`-Verben (inkl. `/umount /undump /settings`).
7. `!l`⇥ → `!ls`/`!load`-Kandidaten.
8. `wh`⇥ (bare) → Monitor-Verben (`whowrote` …).
9. `/mount `⇥ (mit Leerzeichen) → Pfad-Complete im CWD, Kandidaten **farbig** nach Typ.
10. `!cd `⇥ → Verzeichnis-Kandidaten (blau/bold).

## C. Pfad-Complete durch Quotes + Spaces
11. `/mount "` dann Anfang eines Pfads mit Leerzeichen (z.B. `/Users/alex/Development/C64/Cracking/`)⇥
    → completet durch das Quote + Verzeichnisse mit Leerzeichen im Namen; mehrere → farbige Liste,
    gemeinsamer Prefix wird gefüllt.

## D. Zeilen-Editor (immer)
12. Tippe `mount foo`, ← ← ← ← ←, Backspace/Delete/Insert mitten im String → Cursor bewegt sich,
    editiert an der Stelle (kein Append-only).
13. Ctrl-A → Zeilenanfang; Ctrl-E → Ende; Ctrl-K → bis Ende killen; Ctrl-U → bis Anfang;
    Ctrl-W → Wort davor; Ctrl-L → Log leeren.
14. Volle Zeile + Ctrl-C → Zeile gelöscht (nicht quit). Leere Zeile + Ctrl-C → quit.

## E. Persistente History
15. Ein paar Befehle absetzen, `/quit`, `trx64cli` neu starten, ↑ → alte Befehle aus voriger
    Session da (Datei `$HOME/.trx64/history`). Doppelte aufeinanderfolgende Befehle dedupliziert.

## F. Filetype-Farben (LS_COLORS-lite)
16. `!cd` in ein Verzeichnis mit gemischten Typen, `!ls` → Dirs blau/bold, `.crt` gelb,
    `.d64/.g64/.p64` cyan, `.prg/.bin` grün, `.c64re*` magenta, `.asm/.tass/.md/.json` grau.

## G. Medien-Semantik (CRT ≠ Disk) — der Kern deiner Beschwerde
17. **Disk hot-swap:** ein laufendes Programm, dann
    `/mount /Users/alex/Development/C64/Cracking/Murder/motm.g64` ⏎
    → **kein Reset**, C64 läuft weiter, nur Medium getauscht.
18. **CRT power-cycle:** `/window` (Fenster auf), dann
    `/mount <irgendeine .crt>` ⏎ (z.B. eine EF-CRT)
    → **power off → insert → cold boot**, das Ding startet SICHTBAR (Pump resumt, Screen ändert sich).
    Das war der Bug „C64 läuft weiter wie ohne CRT" — jetzt bootet es.
19. **CRT eject:** `/eject` ⏎ → Cart raus, power-cycle, zurück zum normalen C64 (RAM gewiped, wie echt).
20. **/eject smart:** nur Disk gemountet (kein Cart) → `/eject` wirft die Disk, nicht den (fehlenden) Cart.

## H. Aliase + Settings
21. `/umount` == `/eject`; `/undump <p>` == `/restore <p>`.
22. `/settings` ⏎ → read-only Status (pacing / warp / joystick / disk / cart).

---
**Wenn was hakt:** Prozess neu starten (laufender Prozess hält altes Binary im RAM).
Bugs → notieren, ich fix im Folge-Durchlauf. Bekannt & bewusst: bare-Verb-Complete nur
kuratierte Monitor-Liste (kein Live-Abgleich); Verb-Complete case-sensitiv (Pfade case-insensitiv).
