; turbomeas.prg - turbo reference measurements on a real Ultimate (T29).
; Driven over REST: write params + CMD into $C000.., poll SEQ, read results.
;
;   CMD $C000: 1 = measure, 2 = IRQ latency, 3 = monitor (until CMD != 3)
;   Program clears CMD and increments SEQ ($C00F) when a command is done.
;
; Assemble: tools/64tass.sh -a -o turbomeas.prg turbomeas.asm

CMD     = $c000
P_D031  = $c001         ; value to write to $D031
P_WR    = $c002         ; != 0: write P_D031 first
P_D011  = $c003         ; value for $D011
P_TEN   = $c004         ; tenths of a second to measure
P_K     = $c005         ; 256-loop repetitions per outer iteration
P_LINE  = $c006         ; raster line for the IRQ test
P_SAMP  = $c007         ; samples for the IRQ test
P_POKE  = $c008         ; monitor: != 0 -> write P_POKEV to $D031, clear
P_POKEV = $c009
READY   = $c00e         ; $A5 once initialised
SEQ     = $c00f
R_OUT   = $c010         ; 3 bytes outer iterations
R_FRM   = $c013         ; 2 bytes frames (D011 bit 7 1->0)
R_T0    = $c015         ; 4 bytes CIA2 B hi, B lo, A hi, A lo at start
R_T1    = $c019         ; same at end
R_D031B = $c01d         ; $D031 read before the write
R_D031A = $c01e         ; $D031 read after the write
R_D030  = $c01f
M_IDX   = $c020         ; monitor ring index (0..63)
M_CNT   = $c021         ; monitor entries total
R_TODHI = $c022         ; TOD seconds at end (sanity)
B_REFLO = $c100         ; IRQ test: timer at poll-seen line start
B_REFHI = $c200
B_IRQLO = $c300         ; timer at IRQ entry (first instruction)
B_IRQHI = $c400
B_IRQLN = $c500         ; $D012 at IRQ entry
M_RING  = $c600         ; monitor ring: out lo, mid, hi, $D031

outer   = $f0           ; 3 bytes
frames  = $f3           ; 2
lastb7  = $f5
lastt   = $f6
remain  = $f7
kk      = $f8
tmp     = $f9           ; 4
zline   = $fd
flag    = $fe
idx     = $ff

        * = $0801
        .word nxt, 10
        .byte $9e
        .text "2064"
        .byte 0
nxt     .word 0

        * = $0810
start   sei
        lda #$7f
        sta $dc0d
        sta $dd0d
        lda $dc0d
        lda $dd0d
        lda #0
        sta $d01a
        lda #$ff
        sta $d019
        ; TOD on CIA1: 50 Hz input, set (not alarm), start at 0
        lda $dc0f
        and #$7f
        sta $dc0f
        lda $dc0e
        ora #$80
        sta $dc0e
        lda #0
        sta $dc0b
        sta $dc0a
        sta $dc09
        sta $dc08
        jsr cia2free
        lda #0
        sta CMD
        sta SEQ
        sta M_IDX
        sta M_CNT
        lda #$a5
        sta READY

idle    lda CMD
        beq idle
        cmp #1
        bne +
        jmp measure
+       cmp #2
        bne +
        jmp irqlat
+       cmp #3
        bne done
        jmp monitor
done    lda #0
        sta CMD
        inc SEQ
        jmp idle

; CIA2 A free running from $FFFF, B counts A underflows from $FFFF
cia2free
        lda #0
        sta $dd0e
        sta $dd0f
        lda #$ff
        sta $dd04
        sta $dd05
        sta $dd06
        sta $dd07
        lda #$51                ; B: count A underflows, force load, start
        sta $dd0f
        lda #$11                ; A: phi2, continuous, force load, start
        sta $dd0e
        rts

setd031 lda $d031
        sta R_D031B
        lda $d030
        sta R_D030
        lda P_WR
        beq +
        lda P_D031
        sta $d031
+       lda $d031
        sta R_D031A
        rts

measure jsr setd031
        lda P_D011
        sta $d011
        jsr count
        jmp done

; count outer iterations (K * 256 x DEX/BNE) for P_TEN TOD tenths
count   lda #0
        sta outer
        sta outer+1
        sta outer+2
        sta frames
        sta frames+1
        lda $dc08
-       cmp $dc08
        beq -
        lda $dc08
        sta lastt
        ldx #0
        jsr rdtim
        lda $d011
        and #$80
        sta lastb7
        lda P_TEN
        sta remain
        lda P_K
        sta kk
oloop   ldy kk
yl      ldx #0
xl      dex
        bne xl
        dey
        bne yl
        inc outer
        bne +
        inc outer+1
        bne +
        inc outer+2
+       lda $d011
        and #$80
        cmp lastb7
        beq nof
        sta lastb7
        and #$80
        bne nof
        inc frames
        bne nof
        inc frames+1
nof     lda $dc08
        cmp lastt
        beq oloop
        sta lastt
        dec remain
        bne oloop
        ldx #4
        jsr rdtim
        lda outer
        sta R_OUT
        lda outer+1
        sta R_OUT+1
        lda outer+2
        sta R_OUT+2
        lda frames
        sta R_FRM
        lda frames+1
        sta R_FRM+1
        lda $dc09
        sta R_TODHI
        rts

; consistent 32-bit read of CIA2 B:A into R_T0+x
rdtim   lda $dd07
        sta R_T0,x
        lda $dd06
        sta R_T0+1,x
        lda $dd05
        sta R_T0+2,x
        lda $dd04
        sta R_T0+3,x
        lda $dd05
        cmp R_T0+2,x
        bne rdtim
        lda $dd07
        cmp R_T0,x
        bne rdtim
        rts

; IRQ latency: CIA2 A phase-locked to the frame (period 19656), compare the
; timer seen when polling finds the line with the timer at IRQ entry.
irqlat  jsr setd031
        lda #$35
        sta $01
        lda #<irqh
        sta $fffe
        lda #>irqh
        sta $ffff
        lda #<nmih
        sta $fffa
        lda #>nmih
        sta $fffb
        lda #0
        sta $dd0e
        lda #<19655
        sta $dd04
        lda #>19655
        sta $dd05
        lda #$11
        sta $dd0e
        lda P_LINE
        sta zline
        sta $d012
        lda P_D011
        and #$7f
        sta $d011
        ldx #0
        stx idx
sloop   ldx idx
        lda zline
w1      cmp $d012
        beq w1
w2      lda $d012
        cmp zline
        bne w2
        lda $dd04
        ldy $dd05
        sta B_REFLO,x
        tya
        sta B_REFHI,x
        lda #0
        sta flag
        lda #$ff
        sta $d019
        lda #1
        sta $d01a
        cli
w3      lda flag
        beq w3
        sei
        inc idx
        lda idx
        cmp P_SAMP
        bne sloop
        lda #0
        sta $d01a
        lda #$ff
        sta $d019
        lda #$37
        sta $01
        jsr cia2free
        jmp done

irqh    lda $dd04
        ldx idx
        sta B_IRQLO,x
        lda $dd05
        sta B_IRQHI,x
        lda $d012
        sta B_IRQLN,x
        lda #$ff
        sta $d019
        lda #0
        sta $d01a
        inc flag
nmih    rti

; monitor: repeated count of P_TEN tenths, log outer + $D031 into a ring
monitor lda P_POKE
        beq +
        lda P_POKEV
        sta $d031
        lda #0
        sta P_POKE
+       jsr count
        lda M_IDX
        asl
        asl
        tax
        lda outer
        sta M_RING,x
        lda outer+1
        sta M_RING+1,x
        lda outer+2
        sta M_RING+2,x
        lda $d031
        sta M_RING+3,x
        inc M_CNT
        lda M_IDX
        clc
        adc #1
        and #63
        sta M_IDX
        lda CMD
        cmp #3
        beq monitor
        jmp done
