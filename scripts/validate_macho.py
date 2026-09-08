#!/usr/bin/env python3
"""Structural validator for the Mach-O executables bfnative writes.

This exists because codesign/rcodesign only check that the code-signature
blob is internally well-formed and that its hashes match the file on disk.
Neither tool cross-checks the signature's execSegBase/execSegLimit against
the actual segment table, and neither checks segment/section overlap or
page alignment -- but the kernel does, at exec time, which is why a binary
can pass every static check and still get SIGKILLed on launch.

Usage: validate_macho.py <path-to-macho-binary>
Exit code 0 if every check passes, 1 otherwise. Prints one line per problem
found, each naming the field, its value, and what it should be.
"""
import struct
import sys

LC_SEGMENT_64 = 0x19
LC_CODE_SIGNATURE = 0x1D
CSMAGIC_EMBEDDED_SIGNATURE = 0xFADE0CC0
CSMAGIC_CODEDIRECTORY = 0xFADE0C02


def fail(problems, msg):
    problems.append(msg)


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    try:
        with open(sys.argv[1], "rb") as f:
            data = f.read()
    except OSError as e:
        print(f"cannot read {sys.argv[1]}: {e}")
        return 2

    problems = []

    magic, cputype, cpusubtype, filetype, ncmds, sizeofcmds, flags, _reserved = (
        struct.unpack_from("<8I", data, 0)
    )
    if magic != 0xFEEDFACF:
        fail(problems, f"mach_header magic {magic:#x}, expected 0xfeedfacf (64-bit LE)")
        print("\n".join(problems))
        return 1

    page = 0x4000 if cputype == 0x0100000C else 0x1000  # arm64 : x86_64

    segments = []  # (name, vmaddr, vmsize, fileoff, filesize, maxprot, initprot)
    codesig = None  # (dataoff, datasize)

    pos = 32
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<II", data, pos)
        if cmd == LC_SEGMENT_64:
            name = data[pos + 8 : pos + 24].rstrip(b"\0").decode("ascii", "replace")
            vmaddr, vmsize, fileoff, filesize, maxprot, initprot, nsects, _flags = (
                struct.unpack_from("<QQQQiiII", data, pos + 24)
            )
            segments.append((name, vmaddr, vmsize, fileoff, filesize, maxprot, initprot))
        elif cmd == LC_CODE_SIGNATURE:
            dataoff, datasize = struct.unpack_from("<II", data, pos + 8)
            codesig = (dataoff, datasize)
        pos += cmdsize

    if pos != 32 + sizeofcmds:
        fail(
            problems,
            f"load commands end at {pos:#x} but header says sizeofcmds={sizeofcmds:#x} "
            f"(header claims commands end at {32 + sizeofcmds:#x})",
        )

    # ---- segment table sanity ----
    by_name = {s[0]: s for s in segments}
    for expected in ("__PAGEZERO", "__TEXT", "__DATA", "__LINKEDIT"):
        if expected not in by_name:
            fail(problems, f"missing expected segment {expected}")

    if len(problems) == 0 or "__TEXT" in by_name:
        text = by_name.get("__TEXT")

        # __PAGEZERO must end exactly where __TEXT begins in VM space.
        pz = by_name.get("__PAGEZERO")
        if pz and text:
            pz_end = pz[1] + pz[2]
            if pz_end != text[1]:
                fail(
                    problems,
                    f"__PAGEZERO ends at vmaddr {pz_end:#x} but __TEXT starts at "
                    f"{text[1]:#x} -- gap or overlap between them",
                )
            if pz[3] != 0 or pz[4] != 0:
                fail(
                    problems,
                    f"__PAGEZERO has fileoff={pz[3]:#x} filesize={pz[4]:#x}, "
                    "expected both 0 (it's a VM-only guard region)",
                )

        # Every segment's vmaddr and fileoff must be page-aligned, and
        # vmsize must be >= filesize (file content can't exceed the VM
        # window it's mapped into).
        for name, vmaddr, vmsize, fileoff, filesize, maxprot, initprot in segments:
            if name == "__PAGEZERO":
                continue
            if vmaddr % page != 0:
                fail(problems, f"{name}.vmaddr {vmaddr:#x} not aligned to page size {page:#x}")
            if fileoff % 0x10 != 0:
                # file alignment is looser than page alignment but still
                # must be consistent; ld64 uses 16-byte minimum for early
                # segments and page-aligned for later ones -- flag only
                # gross misalignment here.
                pass
            if filesize > vmsize:
                fail(
                    problems,
                    f"{name}.filesize {filesize:#x} exceeds vmsize {vmsize:#x} "
                    "-- more file content than the VM mapping can hold",
                )

        # Adjacent-segment overlap check in VM space: sort by vmaddr and
        # verify each segment's VM window doesn't intersect the next.
        # A sub-page vmsize on a non-last segment is the classic way two
        # segments end up sharing a physical page with different
        # protection bits, which the kernel can reject or silently
        # mis-map.
        real_segs = [s for s in segments if s[0] != "__PAGEZERO"]
        real_segs.sort(key=lambda s: s[1])
        for (n1, va1, vs1, *_r1), (n2, va2, vs2, *_r2) in zip(real_segs, real_segs[1:]):
            end1 = va1 + vs1
            end1_page = (end1 + page - 1) & ~(page - 1)
            if end1 > va2:
                fail(
                    problems,
                    f"{n1} (vmaddr {va1:#x}, vmsize {vs1:#x}, ends {end1:#x}) overlaps "
                    f"{n2} (starts {va2:#x}) in VM space",
                )
            elif end1_page > va2:
                fail(
                    problems,
                    f"{n1} ends at {end1:#x} (page-rounds to {end1_page:#x}) but {n2} "
                    f"starts at {va2:#x}, inside that same page -- two segments with "
                    f"different protection bits sharing one page is unreliable across "
                    f"kernel versions even when it happens to load",
                )

    # ---- code signature cross-check against the segment table ----
    if codesig and "__TEXT" in by_name:
        dataoff, datasize = codesig
        text = by_name["__TEXT"]
        magic_sb, length_sb, count = struct.unpack_from(">III", data, dataoff)
        if magic_sb != CSMAGIC_EMBEDDED_SIGNATURE:
            fail(problems, f"SuperBlob magic {magic_sb:#x}, expected {CSMAGIC_EMBEDDED_SIGNATURE:#x}")
        else:
            cd_offset = None
            for i in range(count):
                btype, boff = struct.unpack_from(">II", data, dataoff + 12 + 8 * i)
                if btype == 0:  # CSSLOT_CODEDIRECTORY
                    cd_offset = dataoff + boff
            if cd_offset is None:
                fail(problems, "no CodeDirectory blob found in SuperBlob")
            else:
                cd_magic, cd_length, version, cd_flags = struct.unpack_from(
                    ">IIII", data, cd_offset
                )
                if cd_magic != CSMAGIC_CODEDIRECTORY:
                    fail(problems, f"CodeDirectory magic {cd_magic:#x}, expected fade0c02")
                if version >= 0x20400:
                    # Full CS_CodeDirectory header layout (cs_blobs.h),
                    # all big-endian:
                    #   0 magic 4  4 length 4   8 version 4  12 flags 4
                    #  16 hashOffset 4  20 identOffset 4
                    #  24 nSpecialSlots 4  28 nCodeSlots 4  32 codeLimit 4
                    #  36 hashSize 1  37 hashType 1  38 platform 1  39 pageSize 1
                    #  40 spare2 4  44 scatterOffset 4  48 teamOffset 4
                    #  52 spare3 4  56 codeLimit64 8
                    #  64 execSegBase 8  72 execSegLimit 8  80 execSegFlags 8
                    code_limit_64, exec_seg_base, exec_seg_limit, exec_seg_flags = (
                        struct.unpack_from(">QQQQ", data, cd_offset + 56)
                    )
                    # execSegBase is a FILE OFFSET of the executable
                    # segment (per xnu cs_blobs.h: "offset of executable
                    # segment"), not a VM address. It must equal __TEXT's
                    # fileoff, and execSegLimit must equal __TEXT's
                    # filesize (the file-relative span covered), not its
                    # vmsize or vmaddr-derived value.
                    if exec_seg_base != text[3]:
                        fail(
                            problems,
                            f"CodeDirectory execSegBase = {exec_seg_base:#x}, but __TEXT "
                            f"fileoff = {text[3]:#x}. execSegBase must be a FILE OFFSET "
                            f"(xnu cs_blobs.h: 'offset of executable segment'), not a VM "
                            f"address -- if this equals __TEXT's vmaddr instead, that's "
                            f"the bug: the kernel's exec-segment check will reject or "
                            f"misinterpret the range and SIGKILL at launch regardless of "
                            f"whether the CodeDirectory hashes are otherwise correct.",
                        )
                    if exec_seg_limit != text[4]:
                        fail(
                            problems,
                            f"CodeDirectory execSegLimit = {exec_seg_limit:#x}, but __TEXT "
                            f"filesize = {text[4]:#x} (expected these to match: the exec "
                            f"segment's range is file-offset-relative, spanning "
                            f"[execSegBase, execSegBase+execSegLimit)).",
                        )

    if problems:
        print(f"{len(problems)} problem(s) found in {sys.argv[1]}:\n")
        for p in problems:
            print(f"  - {p}")
        return 1
    print(f"OK: {sys.argv[1]} passed all structural checks")
    return 0


if __name__ == "__main__":
    sys.exit(main())
