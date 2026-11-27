# Makes the test files that only Windows can make: disc images written by
# the image writer of Windows, and data compressed by its compression
# interface. They are kept in the repository, in
# crates/boxit-codecs/fixtures, so the tests read the same bytes on every
# system; this script is how they were made and how they are made again.
#
#   pwsh scripts/make-fixtures.ps1           the files that are missing
#   pwsh scripts/make-fixtures.ps1 -Force    all of them anew
#
# The archives of RAR in the same folder (rar4-*.rar, rar5-*.rar) are not
# made here: nothing free writes RAR. They are the test archives of the
# `unrar` crate, version 0.5.8 (https://github.com/muja/unrar.rs, MIT OR
# Apache-2.0), under the names they have here:
#
#   rar4-stored.rar    version.rar
#   rar4-locked.rar    crypted.rar                 password "unrar"
#   rar4-volume.rar    archive.part1.rar
#   rar5-secret.rar    comment-hpw-password.rar    password "password"
#   rar5-solid.rar     solid.rar
#   rar5-unicode.rar   unicode-entry.rar
#   rar5-comment.rar   comment.rar
#
# Nothing here needs the rights of an administrator.

param([switch]$Force)

$ErrorActionPreference = "Stop"
$fixtures = Join-Path $PSScriptRoot "..\crates\boxit-codecs\fixtures"
New-Item -ItemType Directory -Force $fixtures | Out-Null

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.IO.Compression;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

public static class Fixtures
{
    // The bytes of the larger files of the fixtures; the tests make the
    // same ones.
    public static byte[] Pattern(int size)
    {
        var bytes = new byte[size];
        for (int i = 0; i < size; i++) bytes[i] = (byte)((i * 7 + (i >> 8) * 13) % 251);
        return bytes;
    }

    // Numbers that grow evenly, four bytes each: what a table in a file
    // looks like.
    public static byte[] Table(int count)
    {
        var bytes = new byte[count * 4];
        for (int i = 0; i < count; i++) BitConverter.GetBytes(1000 + 12 * i).CopyTo(bytes, i * 4);
        return bytes;
    }

    // What the code of a program looks like to a filter that rewrites
    // calls: instructions that name a place by its distance, among bytes
    // that are none of the kind.
    public static byte[] Code(int size)
    {
        var bytes = new System.Collections.Generic.List<byte>(size + 32);
        long state = 12345;
        while (bytes.Count < size)
        {
            state = (state * 1103515245L + 12345L) & 0x7fffffffL;
            long filler = 3 + state % 13;
            for (long i = 0; i < filler; i++) bytes.Add((byte)(0x50 + (state >> 8) % 8));
            int target = 0x1000 * (int)(1 + (state >> 20) % 40);
            int at = bytes.Count;
            switch ((int)((state >> 16) % 7))
            {
                case 5: bytes.AddRange(new byte[] { 0xF0, 0x83, 0x05 }); break;
                case 0: bytes.Add(0xE8); break;
                case 1: bytes.AddRange(new byte[] { 0x48, 0x8B, 0x05 }); break;
                case 2: bytes.AddRange(new byte[] { 0x48, 0x8D, 0x0D }); break;
                case 3: bytes.AddRange(new byte[] { 0x4C, 0x8D, 0x05 }); break;
                case 4: bytes.AddRange(new byte[] { 0xFF, 0x15 }); break;
                default: bytes.Add(0xE9); break;
            }
            bytes.AddRange(BitConverter.GetBytes(target - at));
        }
        return bytes.GetRange(0, size).ToArray();
    }

    public static byte[] Join(params byte[][] parts)
    {
        var all = new MemoryStream();
        foreach (var part in parts) all.Write(part, 0, part.Length);
        return all.ToArray();
    }

    // Text that repeats itself the way text does.
    public static byte[] Text(int lines)
    {
        var text = new System.Text.StringBuilder();
        for (int i = 0; i < lines; i++)
            text.AppendFormat("{0:x} is the number of a line of this text\n", i * 2654435761L % 1000003);
        return System.Text.Encoding.ASCII.GetBytes(text.ToString());
    }

    public static void Gzip(byte[] bytes, string path)
    {
        using (var file = File.Create(path))
        using (var gzip = new GZipStream(file, CompressionLevel.Optimal))
            gzip.Write(bytes, 0, bytes.Length);
    }

    public static byte[] Drain(object stream, int blockSize, int blocks)
    {
        var source = (IStream)stream;
        var all = new MemoryStream();
        var block = new byte[blockSize];
        IntPtr read = Marshal.AllocHGlobal(4);
        try
        {
            for (int i = 0; i < blocks; i++)
            {
                source.Read(block, blockSize, read);
                all.Write(block, 0, Marshal.ReadInt32(read));
            }
        }
        finally { Marshal.FreeHGlobal(read); }
        return all.ToArray();
    }

    [DllImport("cabinet.dll", SetLastError = true)]
    static extern bool CreateCompressor(uint algorithm, IntPtr routines, out IntPtr handle);
    [DllImport("cabinet.dll", SetLastError = true)]
    static extern bool Compress(IntPtr handle, byte[] input, IntPtr inputSize, byte[] output, IntPtr outputSize, out IntPtr written);
    [DllImport("cabinet.dll", SetLastError = true)]
    static extern bool CloseCompressor(IntPtr handle);
    [DllImport("cabinet.dll", SetLastError = true)]
    static extern bool SetCompressorInformation(IntPtr handle, int kind, ref uint value, IntPtr size);

    // One block compressed without the header the interface puts before
    // its own streams: the bytes as an image of Windows holds a chunk.
    public static byte[] Raw(uint algorithm, byte[] input)
    {
        const uint RAW = 1u << 29;
        IntPtr handle;
        if (!CreateCompressor(algorithm | RAW, IntPtr.Zero, out handle))
            throw new System.ComponentModel.Win32Exception();
        try
        {
            // The size of a block (1) is 64 KiB unless more is asked for.
            uint block = (uint)input.Length;
            if (block > 65536 && !SetCompressorInformation(handle, 1, ref block, (IntPtr)4))
                throw new System.ComponentModel.Win32Exception();
            var output = new byte[input.Length + 4096];
            IntPtr written;
            if (!Compress(handle, input, (IntPtr)input.Length, output, (IntPtr)output.Length, out written))
                throw new System.ComponentModel.Win32Exception();
            Array.Resize(ref output, (int)written);
            return output;
        }
        finally { CloseCompressor(handle); }
    }
}
'@

# The tree of the disc images.
$tree = Join-Path ([IO.Path]::GetTempPath()) "boxit-fixtures-tree"
Remove-Item -Recurse -Force $tree -ErrorAction SilentlyContinue
$long = "a name that is longer than the sixty-four characters a name of Joliet may have.txt"
New-Item -ItemType Directory -Force "$tree\docs", "$tree\empty", "$tree\deep\a\b" | Out-Null
[IO.File]::WriteAllText("$tree\readme.txt", "read me`n")
[IO.File]::WriteAllText("$tree\docs\привет мир.txt", "привет")
[IO.File]::WriteAllText("$tree\docs\$long", "long")
[IO.File]::WriteAllBytes("$tree\big.bin", [Fixtures]::Pattern(70000))
[IO.File]::WriteAllBytes("$tree\empty.txt", @())
[IO.File]::WriteAllText("$tree\deep\a\b\leaf.txt", "leaf")
# 2023-11-14 22:13:20 UTC for everything.
$moment = [DateTime]::new(2023, 11, 14, 22, 13, 20, [DateTimeKind]::Utc)
Get-ChildItem $tree -Recurse | ForEach-Object { $_.LastWriteTimeUtc = $moment; $_.CreationTimeUtc = $moment }

# The name of the image, the file systems in it (1 ISO 9660, 2 Joliet,
# 4 UDF) and the revision of UDF.
$images = @(
    @("joliet", 3, 0x102),
    @("udf-102", 4, 0x102),
    @("udf-150", 4, 0x150),
    @("udf-200", 4, 0x200),
    @("udf-201", 4, 0x201),
    @("udf-250", 4, 0x250),
    @("bridge", 7, 0x102)
)
foreach ($image in $images) {
    $path = Join-Path $fixtures "$($image[0]).iso.gz"
    if ((Test-Path $path) -and -not $Force) { continue }
    $writer = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
    $writer.FileSystemsToCreate = $image[1]
    if ($image[1] -band 4) { $writer.UDFRevision = $image[2] }
    $writer.VolumeName = "BOXIT"
    $writer.Root.AddTree($tree, $false)
    $result = $writer.CreateResultImage()
    $bytes = [Fixtures]::Drain($result.ImageStream, $result.BlockSize, $result.TotalBlocks)
    [Fixtures]::Gzip($bytes, $path)
    "{0,-10} {1,9} bytes" -f $image[0], $bytes.Length
}
Remove-Item -Recurse -Force $tree

# Chunks as an image of Windows compresses them: XPRESS with Huffman codes
# (4) and LZMS (5). The tests make the same data and compare. XPRESS takes
# no more than 64 KiB at a time.
$text = [Fixtures]::Text(1200)
$plain = [ordered]@{
    "text"    = [Fixtures]::Text(700)
    "pattern" = [Fixtures]::Pattern(32768)
    "zeros"   = (New-Object byte[] 32768)
    "short"   = [Text.Encoding]::ASCII.GetBytes("abcabcabcabcabcabc, and again: abcabcabcabcabcabc")
    "code"    = [Fixtures]::Code(48000)
    "table"   = [Fixtures]::Table(8000)
    "mixed"   = [Fixtures]::Join($text, [Fixtures]::Pattern(20000), [Fixtures]::Table(8000), [Fixtures]::Code(20000), $text)
    "large"   = [Fixtures]::Join([Fixtures]::Text(30000), [Fixtures]::Code(400000), [Fixtures]::Text(30000))
}
foreach ($name in $plain.Keys) {
    foreach ($method in @(@("xpress", 4), @("lzms", 5))) {
        $path = Join-Path $fixtures "$name.$($method[0])"
        if ($method[0] -eq "xpress" -and $plain[$name].Length -gt 65536) { continue }
        if ((Test-Path $path) -and -not $Force) { continue }
        $packed = [Fixtures]::Raw($method[1], $plain[$name])
        [IO.File]::WriteAllBytes($path, $packed)
        "{0,-10} {1,-7} {2,7} -> {3,7} bytes" -f $name, $method[0], $plain[$name].Length, $packed.Length
    }
}
