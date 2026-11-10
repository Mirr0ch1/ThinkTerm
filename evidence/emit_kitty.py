#!/usr/bin/env python3
"""Emit kitty-graphics images the way oh-my-pi (omp) does.

omp transmits a PNG under a 24-bit image id (a=t,f=100,i=ID), reserves the
component's height with blank lines, then places the image over the last
rows of that block (a=p,q=2,C=1,i=ID,p=PID,c=COLS,r=ROWS) and restores the
cursor to the block's bottom. It deletes stale placements
(a=d,d=i,i=ID,p=PID) as they scroll out of its viewport.
"""
import base64
import os
import sys
import time


def chunks(data, size=4096):
    for i in range(0, len(data), size):
        yield data[i:i + size], i + size >= len(data)


def transmit(png: bytes, image_id: int):
    payload = base64.b64encode(png).decode()
    out = []
    pieces = list(chunks(payload))
    for index, (piece, last) in enumerate(pieces):
        if index == 0:
            control = f"a=t,f=100,q=2,i={image_id},m={0 if last else 1}"
            out.append(f"\x1b_G{control};{piece}\x1b\\")
        else:
            out.append(f"\x1b_Gq=2,m={0 if last else 1};{piece}\x1b\\")
    sys.stdout.write("".join(out))


def place(image_id: int, placement_id: int, cols: int, rows: int, y=None, h=None):
    control = f"a=p,q=2,C=1,i={image_id},p={placement_id},c={cols},r={rows}"
    if y is not None and h is not None:
        control += f",y={y},h={h}"
    return f"\x1b_G{control}\x1b\\"


def emit_component(png: bytes, image_id: int, placement_id: int, cols: int = 48, rows: int = 20):
    transmit(png, image_id)
    seq = place(image_id, placement_id, cols, rows)
    if rows > 1:
        sys.stdout.write("\n" * (rows - 1))
        sys.stdout.write("\x1b7\x1b[%dA%s\x1b8\r\n" % (rows - 1, seq))
    else:
        sys.stdout.write(seq)


def delete_placement(image_id: int, placement_id: int):
    sys.stdout.write(f"\x1b_Ga=d,d=i,i={image_id},p={placement_id},q=2\x1b\\")


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "plain"
    total = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    here = os.path.dirname(os.path.abspath(__file__))
    base = 0x120000
    placements = []
    for i in range(1, total + 1):
        png = open(os.path.join(here, f"img{i}.png"), "rb").read()
        image_id = (base + i * 0x10000) & 0xFFFFFF
        placement_id = image_id
        emit_component(png, image_id, placement_id)
        sys.stdout.write(f"--- image {i} above ---\n")
        sys.stdout.flush()
        placements.append((image_id, placement_id))
        time.sleep(0.05)
        if mode == "delete-stale" and len(placements) > 2:
            old_id, old_pid = placements.pop(0)
            delete_placement(old_id, old_pid)
            sys.stdout.flush()
        if mode == "evict" and len(placements) > 8:
            old_id, old_pid = placements.pop(0)
            delete_placement(old_id, old_pid)
            sys.stdout.write(f"\x1b_Ga=d,d=I,i={old_id},q=2\x1b\\")
            sys.stdout.flush()
    sys.stdout.write("=== done ===\n")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
