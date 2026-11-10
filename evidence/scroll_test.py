#!/usr/bin/env python3
"""Print one large kitty image, scroll it far into the scrollback, optionally
delete it the way omp's tui.maxInlineImages budget does, then keep the pane
alive so the scrollback can be inspected."""
import base64
import os
import sys


def main():
    delete = len(sys.argv) > 1 and sys.argv[1] == "delete"
    lines_after = int(sys.argv[2]) if len(sys.argv) > 2 else 4000
    here = os.path.dirname(os.path.abspath(__file__))
    png = open(os.path.join(here, "stripes.png"), "rb").read()
    image_id = 0x550001
    placement_id = 0x550001
    payload = base64.b64encode(png).decode()
    out = []
    for i in range(0, len(payload), 4096):
        piece = payload[i:i + 4096]
        last = i + 4096 >= len(payload)
        if i == 0:
            out.append(f"\x1b_Ga=t,f=100,q=2,i={image_id},m={0 if last else 1};{piece}\x1b\\")
        else:
            out.append(f"\x1b_Gq=2,m={0 if last else 1};{piece}\x1b\\")
    sys.stdout.write("".join(out))
    sys.stdout.write("\x1b[2J\x1b[H")
    sys.stdout.write("BEFORE-IMAGE\r\n")
    sys.stdout.write(f"\x1b_Ga=p,q=2,C=1,i={image_id},p={placement_id},c=48,r=20\x1b\\")
    sys.stdout.write("\r\n" * 19)
    sys.stdout.write("AFTER-IMAGE\r\n")
    for i in range(30):
        sys.stdout.write(f"filler {i:02d}\r\n")
    if delete:
        sys.stdout.write(f"\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
        sys.stdout.write("DELETED-IMAGE-DATA\r\n")
    for i in range(lines_after):
        sys.stdout.write(f"line {i:04d}\r\n")
    sys.stdout.write("END\r\n")
    sys.stdout.flush()


main()
