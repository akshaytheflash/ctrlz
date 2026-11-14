$ErrorActionPreference = 'Stop'

# ctrlz - Chocolatey install script.
#
# The values below are placeholders that CI substitutes with the matching
# GitHub Release archive before `choco pack` runs:
#
#   {{URL}}       absolute download URL for ctrlz-x86_64-pc-windows-msvc.zip
#   {{CHECKSUM}}  sha256 of that zip, substituted by CI at pack time
#
# Nothing is fetched while packing, and this script never prompts: it unpacks
# the single portable executable into the package's own tools directory, so it
# needs no administrator rights of its own.

$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

$packageArgs = @{
    packageName    = 'ctrlz'
    url64bit       = '{{URL}}'
    checksum64     = '{{CHECKSUM}}'
    checksumType64 = 'sha256'
    unzipLocation  = $toolsDir
}

Install-ChocolateyZipPackage @packageArgs
