param(
    [Parameter(Mandatory = $true)]
    [string] $Action
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2
$ApplicationId = '55c92734-d682-4d71-983e-d6ec3f16059f'

function Get-WindowsProducts {
    Get-CimInstance -ClassName SoftwareLicensingProduct | Where-Object {
        $_.ApplicationID -eq $ApplicationId -and
        $_.Name -like 'Windows*' -and
        $null -ne $_.PartialProductKey
    }
}

function Write-LswStatus {
    param([Parameter(Mandatory = $true)][string] $Value)
    [Console]::Out.WriteLine($Value)
}

try {
    switch ($Action) {
        'status' {
            $Product = Get-WindowsProducts |
                Sort-Object -Property LicenseStatus -Descending |
                Select-Object -First 1
            if ($null -eq $Product -or $Product.LicenseStatus -ne 1) {
                Write-LswStatus 'STATUS=unlicensed'
            } else {
                Write-LswStatus 'STATUS=licensed'
            }
            if ($null -ne $Product) {
                Write-LswStatus ('LICENSE_STATUS={0}' -f $Product.LicenseStatus)
            }
        }
        'activate' {
            $Key = [Console]::In.ReadLine()
            try {
                if ($null -eq $Key -or $Key -notmatch '^[A-Z0-9]{5}(-[A-Z0-9]{5}){4}$') {
                    throw 'Invalid product key input.'
                }
                $PartialKey = $Key.Substring($Key.Length - 5)
                $Service = Get-CimInstance -ClassName SoftwareLicensingService
                $Install = Invoke-CimMethod -InputObject $Service -MethodName InstallProductKey -Arguments @{
                    ProductKey = $Key
                }
                if ($Install.ReturnValue -ne 0) { throw 'InstallProductKey failed.' }
                $Product = Get-WindowsProducts | Where-Object {
                    $_.PartialProductKey -eq $PartialKey
                } | Select-Object -First 1
                if ($null -eq $Product) { throw 'Installed Windows product was not found.' }
                $Activation = Invoke-CimMethod -InputObject $Product -MethodName Activate
                if ($Activation.ReturnValue -ne 0) { throw 'Activate failed.' }
            } finally {
                $Key = $null
            }
            Write-LswStatus 'STATUS=activation-requested'
        }
        'online' {
            $Product = Get-WindowsProducts |
                Sort-Object -Property LicenseStatus -Descending |
                Select-Object -First 1
            if ($null -eq $Product) { throw 'No installed Windows product key was found.' }
            $Activation = Invoke-CimMethod -InputObject $Product -MethodName Activate
            if ($Activation.ReturnValue -ne 0) { throw 'Activate failed.' }
            Write-LswStatus 'STATUS=activation-requested'
        }
        default {
            throw 'Unsupported activation action.'
        }
    }
} catch {
    exit 1
}
