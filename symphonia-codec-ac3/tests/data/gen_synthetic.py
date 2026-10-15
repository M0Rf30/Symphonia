#!/usr/bin/env python3
"""Generate synthetic AC-3 syncframes with every bin at bap 15 (16-bit mantissas).

csnroffst=63, fsnroffst=15, floorcod=7 saturate the bit allocation so that every bin gets bap 15
(ETSI TS 102 366 clause 7.2.2.7), which makes the mantissa bit count independent of the masking
model. Exponents are flat. Used for fixtures covering acmod 0 (dual mono), LFE, dynrng.
"""
import random, struct, sys

class BW:
    def __init__(s): s.bits = []
    def put(s, v, n):
        for i in range(n - 1, -1, -1): s.bits.append((v >> i) & 1)
    def tobytes(s):
        b = s.bits + [0] * (-len(s.bits) % 8)
        return bytes(int(''.join(map(str, b[i:i+8])), 2) for i in range(0, len(b), 8))

def crc16(data, crc=0):
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x8005) & 0xffff if crc & 0x8000 else (crc << 1) & 0xffff
    return crc

def solve_crc1(buf, k):
    # Find X so that crc16(X || buf[4:k]) == 0 (linear in X).
    base = crc16(bytes([0, 0]) + bytes(buf[4:k]))
    cols = []
    for i in range(16):
        x = 1 << i
        cols.append(crc16(bytes([x >> 8, x & 255]) + bytes(k - 4)))
    # Gaussian elimination: find x with xor of cols[i] for set bits == base.
    rows = [(cols[i], 1 << i) for i in range(16)]
    basis = []
    for c, m in rows:
        for bc, bm in basis:
            if c ^ bc < c: c ^= bc; m ^= bm
        if c: basis.append((c, m)); basis.sort(reverse=True)
    t, x = base, 0
    for bc, bm in basis:
        if t ^ bc < t: t ^= bc; x ^= bm
    assert t == 0
    return x

BITRATES = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640]

def frame(acmod, lfeon, exps, dyn, rng, chbw=0, bitrate_idx=18, fscod=0, dither=1):
    nfbw = [2, 1, 2, 3, 3, 4, 4, 5][acmod]
    frmsizecod = 2 * bitrate_idx
    nbytes = {0: BITRATES[bitrate_idx] * 4, 2: BITRATES[bitrate_idx] * 6}[fscod]
    w = BW()
    w.put(0x0b77, 16); w.put(0, 16); w.put(fscod, 2); w.put(frmsizecod, 6)
    w.put(8, 5); w.put(0, 3); w.put(acmod, 3)
    if acmod & 1 and acmod != 1: w.put(1, 2)       # cmixlev
    if acmod & 4: w.put(1, 2)                      # surmixlev
    if acmod == 2: w.put(0, 2)                     # dsurmod
    w.put(lfeon, 1)
    for _ in range(2 if acmod == 0 else 1):
        w.put(31, 5); w.put(0, 1); w.put(0, 1); w.put(0, 1)  # dialnorm, compre, langcode, audprodie
    w.put(0, 1); w.put(0, 1); w.put(0, 1); w.put(0, 1)       # copyright, origbs, timecod1e, timecod2e
    w.put(0, 1)                                              # addbsie
    endmant = 37 + 3 * (chbw + 12)
    for blk in range(6):
        for _ in range(nfbw): w.put(0, 1)                    # blksw
        for _ in range(nfbw): w.put(dither, 1)               # dithflag
        for i in range(2 if acmod == 0 else 1):
            if blk == 0: w.put(1, 1); w.put(dyn[i], 8)
            else: w.put(0, 1)
        w.put(1 if blk == 0 else 0, 1)                       # cplstre
        if blk == 0: w.put(0, 1)                             # cplinu
        if acmod == 2: w.put(0, 1)                           # rematstr
        for ch in range(nfbw): w.put(1 if blk == 0 else 0, 2)  # D15 / reuse
        if lfeon: w.put(1 if blk == 0 else 0, 1)
        if blk == 0:
            for ch in range(nfbw): w.put(chbw, 6)
            for ch in range(nfbw):
                w.put(exps[ch], 4)
                for _ in range((endmant - 1) // 3): w.put(62, 7)
                w.put(0, 2)                                   # gainrng
            if lfeon:
                w.put(exps[nfbw], 4)
                for _ in range(2): w.put(62, 7)
        if blk == 0:
            w.put(1, 1); w.put(0, 2); w.put(0, 2); w.put(0, 2); w.put(0, 2); w.put(7, 3)  # baie
            w.put(1, 1); w.put(63, 6)
            for ch in range(nfbw): w.put(15, 4); w.put(7, 3)
            if lfeon: w.put(15, 4); w.put(7, 3)
        else:
            w.put(0, 1); w.put(0, 1)
        w.put(0, 1)                                           # deltbaie
        w.put(0, 1)                                           # skiple
        for ch in range(nfbw):
            for _ in range(endmant): w.put(rng.randrange(65536), 16)
        if lfeon:
            for _ in range(7): w.put(rng.randrange(65536), 16)
    body = w.tobytes()
    assert len(body) + 2 <= nbytes, (len(body), nbytes)
    buf = bytearray(body + bytes(nbytes - len(body)))
    k = 2 * ((nbytes // 2) * 5 // 8 + (1 if (nbytes // 2) * 5 % 8 else 0))
    x = solve_crc1(buf, k)
    buf[2] = x >> 8; buf[3] = x & 255
    c = crc16(bytes(buf[2:nbytes - 2]))
    buf[nbytes - 2] = c >> 8; buf[nbytes - 1] = c & 255
    assert crc16(bytes(buf[2:k])) == 0 and crc16(bytes(buf[2:])) == 0
    return bytes(buf)

def eframe(rng, acmod, lfeon, exps, nbytes, fscod2=0, chbw=0, transient=None, fscod=3):
    """One E-AC-3 independent syncframe: six blocks, reduced sample rate `fscod2` if `fscod` is 3.

    `transient` is a per-channel list of None or (transprocloc, transproclen).
    """
    nfbw = [2, 1, 2, 3, 3, 4, 4, 5][acmod]
    w = BW()
    w.put(0x0b77, 16)
    w.put(0, 2); w.put(0, 3); w.put(nbytes // 2 - 1, 11)      # strmtyp, substreamid, frmsiz
    # fscod 3 selects the reduced rate `fscod2`; otherwise numblkscod 3 (six blocks) follows.
    w.put(fscod, 2); w.put(fscod2 if fscod == 3 else 3, 2)
    w.put(acmod, 3); w.put(lfeon, 1); w.put(16, 5)            # acmod, lfeon, bsid
    for _ in range(2 if acmod == 0 else 1):
        w.put(31, 5); w.put(0, 1)                             # dialnorm, compre
    w.put(0, 1); w.put(0, 1); w.put(0, 1)                     # mixmdate, infomdate, addbsie
    # audfrm
    w.put(1, 1); w.put(0, 1)                                  # expstre, ahte
    w.put(0, 2)                                               # snroffststr
    w.put(1 if transient else 0, 1)                           # transproce
    w.put(0, 1); w.put(0, 1); w.put(0, 1)                     # blkswe, dithflage, bamode
    w.put(0, 1); w.put(0, 1); w.put(0, 1); w.put(0, 1)        # frmfgaincode, dbaflde, skipflde, spxattene
    if acmod > 1:
        w.put(0, 1)                                           # cplinu[0]
        for _ in range(1, 6): w.put(0, 1)                     # cplstre[blk]
    for blk in range(6):
        for _ in range(nfbw): w.put(1 if blk == 0 else 0, 2)  # chexpstr
    if lfeon:
        for blk in range(6): w.put(1 if blk == 0 else 0, 1)   # lfeexpstr
    for _ in range(nfbw): w.put(0, 5)                         # convexpstr
    w.put(63, 6); w.put(15, 4)                                # frmcsnroffst, frmfsnroffst
    if transient:
        for ch in range(nfbw):
            t = transient[ch]
            if t: w.put(1, 1); w.put(t[0], 10); w.put(t[1], 8)
            else: w.put(0, 1)
    w.put(0, 1)                                               # blkstrtinfoe
    endmant = 37 + 3 * (chbw + 12)
    for blk in range(6):
        w.put(0, 1)                                           # dynrnge
        if acmod == 0: w.put(0, 1)                            # dynrng2e
        if blk > 0: w.put(0, 1)                               # spxstre
        else: w.put(0, 1)                                     # spxinu
        if acmod == 2:
            if blk == 0:
                for _ in range(4): w.put(0, 1)                # rematflg
            else: w.put(0, 1)                                 # rematstr
        if blk == 0:
            for ch in range(nfbw): w.put(chbw, 6)
            for ch in range(nfbw):
                w.put(exps[ch], 4)
                for _ in range((endmant - 1) // 3): w.put(62, 7)
                w.put(0, 2)                                   # gainrng
            if lfeon:
                w.put(exps[nfbw], 4)
                for _ in range(2): w.put(62, 7)
        w.put(0, 1)                                           # convsnroffste
        for ch in range(nfbw):
            for _ in range(endmant): w.put(rng.randrange(65536), 16)
        if lfeon:
            for _ in range(7): w.put(rng.randrange(65536), 16)
    body = w.tobytes()
    assert len(body) + 2 <= nbytes, (len(body), nbytes)
    buf = bytearray(body + bytes(nbytes - len(body)))
    c = crc16(bytes(buf[2:nbytes - 2]))
    buf[nbytes - 2] = c >> 8; buf[nbytes - 1] = c & 255
    return bytes(buf)

def stream(nframes, seed=1, **kw):
    rng = random.Random(seed)
    return b''.join(frame(rng=rng, **kw) for _ in range(nframes))

if __name__ == '__main__':
    out = sys.argv[1]
    acmod, lfe = int(sys.argv[2]), int(sys.argv[3])
    exps = [int(x) for x in sys.argv[4].split(',')]
    dyn = [int(x, 0) for x in sys.argv[5].split(',')]
    open(out, 'wb').write(stream(4, acmod=acmod, lfeon=lfe, exps=exps, dyn=dyn))
