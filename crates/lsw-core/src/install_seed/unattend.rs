// SPDX-License-Identifier: GPL-3.0-or-later

use super::InstallSeedOptions;
use crate::{InstanceManifest, LswError, Result};

pub(super) const SETUP_ACCOUNT_NAME: &str = "LSWSetup";

pub(super) fn autounattend(
    manifest: &InstanceManifest,
    options: &InstallSeedOptions,
    setup_account_password_value: &str,
) -> String {
    let selection = options
        .unattended_image_name
        .as_ref()
        .map(|name| ("/IMAGE/NAME", xml_escape(name)))
        .or_else(|| {
            options
                .unattended_image_index
                .map(|index| ("/IMAGE/INDEX", index.to_string()))
        });
    let disk = selection
        .map(|(key, value)| {
            format!(
                r#"
      <DiskConfiguration>
        <Disk wcm:action="add">
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>260</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Label>System</Label><Format>FAT32</Format></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>3</PartitionID><Label>Windows</Label><Letter>C</Letter><Format>NTFS</Format></ModifyPartition>
          </ModifyPartitions>
        </Disk>
        <WillShowUI>OnError</WillShowUI>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallFrom><MetaData wcm:action="add"><Key>{key}</Key><Value>{value}</Value></MetaData></InstallFrom>
          <InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
          <WillShowUI>OnError</WillShowUI>
        </OSImage>
      </ImageInstall>"#
            )
        })
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <SetupUILanguage><UILanguage>{locale}</UILanguage></SetupUILanguage>
      <InputLocale>{locale}</InputLocale><SystemLocale>{locale}</SystemLocale><UILanguage>{locale}</UILanguage><UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">{disk}
      <UserData><AcceptEula>true</AcceptEula><FullName>LSW User</FullName><Organization>LSW</Organization></UserData>
    </component>
  </settings>
  <settings pass="specialize">
    <component name="Microsoft-Windows-Deployment" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order><Description>Install LSW guest services</Description>
          <Path>cmd.exe /d /c for %D in (D E F G H I J K L M N O P Q R S T U V W X Y Z) do @if exist "%D:\lsw\install-agent.ps1" powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%D:\lsw\install-agent.ps1"</Path>
        </RunSynchronousCommand>
      </RunSynchronous>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>{computer_name}</ComputerName>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>{locale}</InputLocale><SystemLocale>{locale}</SystemLocale><UILanguage>{locale}</UILanguage><UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <RegisteredOrganization>LSW</RegisteredOrganization>
      <RegisteredOwner>LSW User</RegisteredOwner>
      <TimeZone>UTC</TimeZone>
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Password><Value>{setup_account_password_value}</Value><PlainText>false</PlainText></Password>
            <Description>Temporary account removed when unattended setup completes</Description>
            <DisplayName>LSW Setup</DisplayName><Group>Users</Group><Name>{setup_account_name}</Name>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
    </component>
  </settings>
</unattend>
"#,
        locale = options.locale,
        computer_name = windows_computer_name(&manifest.spec.name),
        setup_account_name = SETUP_ACCOUNT_NAME,
        setup_account_password_value = setup_account_password_value,
    )
}

pub(super) fn generate_setup_account_password() -> Result<String> {
    let mut random = [0_u8; 24];
    getrandom::getrandom(&mut random).map_err(|error| {
        LswError::Io(std::io::Error::other(format!(
            "the operating system random source failed: {error}"
        )))
    })?;

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut password = String::with_capacity(53);
    password.push_str("LsW!9");
    for byte in random {
        password.push(HEX[(byte >> 4) as usize] as char);
        password.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(password)
}

pub(super) fn unattend_password_value(password: &str) -> String {
    let mut bytes = Vec::with_capacity((password.len() + "Password".len()) * 2);
    for code_unit in password.encode_utf16().chain("Password".encode_utf16()) {
        bytes.extend_from_slice(&code_unit.to_le_bytes());
    }
    base64_encode(&bytes)
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

pub(super) fn validate_locale(locale: &str) -> Result<()> {
    if (2..=20).contains(&locale.len())
        && locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && locale.bytes().any(|byte| byte == b'-')
    {
        Ok(())
    } else {
        Err(LswError::InvalidValue {
            field: "locale",
            reason: "must look like en-US or zh-HK".to_owned(),
        })
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn windows_computer_name(instance: &str) -> String {
    let mut name = format!("LSW-{}", instance.to_ascii_uppercase())
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(15)
        .collect::<String>();
    while name.ends_with('-') {
        name.pop();
    }
    name
}
