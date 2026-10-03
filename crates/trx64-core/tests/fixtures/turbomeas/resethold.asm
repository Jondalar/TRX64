; resethold - measure the post-reset 1 MHz hold from the CPU's first instruction.
; Written into RAM at $8000 over REST (machine:writemem) with a CBM80 signature,
; so the KERNAL's reset routine jumps here right after a reset (no IOINIT, VIC off).
; It disarms itself, starts CIA2 as a 32-bit PHI2 tick counter and runs
; 256 x DEX/BNE per outer iteration; the first outer iteration that takes fewer than
; 700 ticks (turbo) stores the tick count since start.
;
;   $8100 != 0: write $8101 to $D031 at start (registers mode)
;   $8110..13 ticks left (B hi, B lo, A hi, A lo; start = $FFFFFFFF) at the change
;   $8114..15 outer iterations before the change
;   $8116..17 first delta, $8118..19 last delta, $811A = $5A when changed
;   $811B counts up while running (alive)

        * = $8000
        .word start, start
        .byte $c3, $c2, $cd, $38, $30

prev    = $f0           ; 2 bytes
cnt     = $f2           ; 2
cur     = $f4           ; 4
dl      = $f8           ; 2

start   sei
        ldx #$ff
        txs
        cld
        lda #0
        sta $8004       ; disarm the signature
        sta $811a
        sta cnt
        sta cnt+1
        sta $dd0e
        sta $dd0f
        lda #$ff
        sta $dd04
        sta $dd05
        sta $dd06
        sta $dd07
        lda #$51
        sta $dd0f
        lda #$11
        sta $dd0e
        lda $8100
        beq +
        lda $8101
        sta $d031
+       jsr rdtim
        lda cur+3
        sta prev
        lda cur+2
        sta prev+1
oloop   ldx #0
xl      dex
        bne xl
        inc cnt
        bne +
        inc cnt+1
+       jsr rdtim
        sec
        lda prev
        sbc cur+3
        sta dl
        lda prev+1
        sbc cur+2
        sta dl+1
        lda cur+3
        sta prev
        lda cur+2
        sta prev+1
        lda dl
        sta $8118
        lda dl+1
        sta $8119
        lda cnt+1       ; store the first delta (outer #1)
        bne +
        lda cnt
        cmp #1
        bne +
        lda dl
        sta $8116
        lda dl+1
        sta $8117
+       inc $811b
        lda $811a
        bne oloop
        lda dl+1
        cmp #>700
        bcc fast
        bne oloop
        lda dl
        cmp #<700
        bcs oloop
fast    ldx #3
-       lda cur,x
        sta $8110,x
        dex
        bpl -
        lda cnt
        sta $8114
        lda cnt+1
        sta $8115
        lda #$5a
        sta $811a
        jmp oloop

rdtim   lda $dd07
        sta cur
        lda $dd06
        sta cur+1
        lda $dd05
        sta cur+2
        lda $dd04
        sta cur+3
        lda $dd05
        cmp cur+2
        bne rdtim
        lda $dd07
        cmp cur
        bne rdtim
        rts
