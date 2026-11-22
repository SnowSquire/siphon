# Fetches and installs Microsoft's Visual C++ 2015-2022 x64 redistributable.
#
# The Siphon installer runs this only when the runtime is absent: the binaries
# use the dynamic CRT, and without it they fail before main. The URL is
# Microsoft's permanent aka.ms link, and the Authenticode check makes sure the
# URL still serves their signed installer before anything is executed.
$ErrorActionPreference = 'Stop'

$dest = Join-Path $env:TEMP 'vc_redist.x64.exe'
Invoke-WebRequest -UseBasicParsing -Uri 'https://aka.ms/vs/17/release/vc_redist.x64.exe' -OutFile $dest

$signature = Get-AuthenticodeSignature -FilePath $dest
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notlike '*O=Microsoft Corporation*') {
    exit 2
}

# Elevates once; the installer itself stays per-user.
$process = Start-Process -FilePath $dest -ArgumentList '/install', '/quiet', '/norestart' -Verb RunAs -Wait -PassThru
exit $process.ExitCode
