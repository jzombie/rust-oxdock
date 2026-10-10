"IMPORT [STD]`nECHO 'via-ephemeral'" | & ([scriptblock]::Create((Get-Content -Raw .\install.ps1)))
