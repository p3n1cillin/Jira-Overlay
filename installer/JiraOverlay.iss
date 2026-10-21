; Stable AppId: the SAME installer installs fresh and upgrades in place.
#ifndef AppVersion
  #error AppVersion must be supplied by build_installer.py
#endif
[Setup]
AppId={{70CD5DF5-2577-42CC-AD73-24419C907568}
AppName=Jira Overlay
AppVersion={#AppVersion}
AppPublisher=Jira Overlay contributors
AppPublisherURL=https://github.com/p3n1cillin/Jira-Overlay
AppSupportURL=https://github.com/p3n1cillin/Jira-Overlay/issues
DefaultDirName={localappdata}\Programs\JiraOverlay
DefaultGroupName=Jira Overlay
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
DisableProgramGroupPage=yes
DisableDirPage=yes
DisableWelcomePage=yes
DirExistsWarning=no
UsePreviousAppDir=yes
UsePreviousTasks=yes
SetupMutex=Local\JiraOverlaySetup
OutputDir={#OutputDir}
OutputBaseFilename=JiraOverlay-{#AppVersion}-Setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
UninstallDisplayName=Jira Overlay
UninstallDisplayIcon={app}\runtime\pythonw.exe
VersionInfoVersion={#AppVersion}
LicenseFile={#StageDir}\LICENSE
DisableReadyPage=yes
Uninstallable=yes
SetupLogging=yes
UninstallLogging=yes

[Tasks]
Name: startup; Description: "Start Jira Overlay and the learner when I sign in"; Flags: checkedonce

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs; Excludes: "release-manifest.json"
Source: "support.ps1"; Flags: dontcopy
Source: "legacy_shutdown.cs"; Flags: dontcopy

[Icons]
Name: "{group}\Jira Overlay"; Filename: "{app}\runtime\pythonw.exe"; Parameters: """{app}\launch_installed.pyw"""; WorkingDir: "{app}"; Check: MakeShortcuts
Name: "{group}\Uninstall Jira Overlay"; Filename: "{uninstallexe}"; Check: MakeShortcuts

[INI]
Filename: "{app}\.jira-overlay-install"; Section: "Install"; Key: "Application"; String: "JiraOverlay"

[Run]
Filename: "{app}\runtime\pythonw.exe"; Parameters: """{app}\launch_installed.pyw"""; WorkingDir: "{app}"; Flags: nowait; Check: LaunchApplication

[Code]
var
  LegacyPage: TInputDirWizardPage;
  ErrorFile, Helper, InstallRoot: String;
  AlreadyInstalled: Boolean;
  PreparationPage: TOutputProgressWizardPage;
  ShowingPreparation: Boolean;

function ReadResult(var Value: String): Boolean;
var Data: AnsiString;
begin
  Result := LoadStringFromFile(ErrorFile, Data);
  if Result then Value := UTF8Decode(Data);
end;

function HasFlag(Name: String): Boolean;
var I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/' + Name) = 0 then Result := True;
end;

function MakeShortcuts: Boolean;
begin Result := not HasFlag('NOSHORTCUTS'); end;

function LaunchApplication: Boolean;
begin Result := not HasFlag('NOLAUNCH'); end;

procedure HelperLog(const S: String; const Error, FirstLine: Boolean);
var MessageText, Number: String; Separator, Percent: Integer;
begin
  Log(S);
  if ShowingPreparation then begin
    MessageText := S;
    if Pos('JIRA_PROGRESS|', S) = 1 then begin
      MessageText := Copy(S, 15, Length(S));
      Separator := Pos('|', MessageText);
      if Separator > 0 then begin
        Number := Copy(MessageText, 1, Separator - 1);
        Percent := StrToIntDef(Number, 0);
        PreparationPage.SetProgress(Percent, 100);
        MessageText := Copy(MessageText, Separator + 1, Length(MessageText));
      end;
    end;
    PreparationPage.SetText(MessageText, 'Closing apps, checking folders and verifying personal data. Your old folder is retained.');
  end;
end;

function RunHelper(Action, Source, Script: String; Startup: Boolean): Boolean;
var Args, ErrorText: String; Code: Integer;
begin
  DeleteFile(ErrorFile);
  Args := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + Script +
    '" -Action ' + Action + ' -Target "' + InstallRoot +
    '" -ResultFile "' + ErrorFile + '" -Version "{#AppVersion}"';
  if Source <> '' then Args := Args + ' -Source "' + Source + '"';
  if Startup then Args := Args + ' -EnableStartup';
  Result := ExecAndLogOutput(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    Args, '', SW_HIDE, ewWaitUntilTerminated, Code, @HelperLog) and (Code = 0);
  if not Result then begin
    if not ReadResult(ErrorText) then begin
      ErrorText := '[JIRA-I900] Installer helper failed (Windows/helper status ' + IntToStr(Code) + '). See the Setup Log in your Windows TEMP folder. ' +
        'If PowerShell was blocked by security policy, ask your IT team. No running learner is force-killed.';
      SaveStringToFile(ErrorFile, UTF8Encode(ErrorText), False);
    end;
    Log(ErrorText);
  end;
end;

procedure InitializeWizard;
var OldPath, RunValue: String;
begin
  ExtractTemporaryFile('support.ps1');
  ExtractTemporaryFile('legacy_shutdown.cs');
  Helper := ExpandConstant('{tmp}\support.ps1');
  ErrorFile := ExpandConstant('{tmp}\jira-installer-result.txt');
  InstallRoot := WizardDirValue;
  AlreadyInstalled := FileExists(InstallRoot + '\unins000.exe');
  PreparationPage := CreateOutputProgressPage('Preparing installation', 'Checking apps and safely copying any selected personal data');
  LegacyPage := CreateInputDirPage(wpSelectDir, 'Previous ZIP installation (optional)',
    'Fresh install or upgrade: the same installer handles both.',
    'If you used a ZIP/clone before, select its folder to COPY your settings and learner data. ' +
    'The old folder and its backups are kept. Leave blank for a new user. Setup will ask any running apps to quit automatically.', False, '');
  LegacyPage.Add('Previous application folder:');
  if RunHelper('Discover', '', Helper, False) then
    if ReadResult(OldPath) then LegacyPage.Values[0] := OldPath;
  LegacyPage.Values[0] := ExpandConstant('{param:LEGACYDIR|' + LegacyPage.Values[0] + '}');
  if AlreadyInstalled then begin
    if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'JiraOverlay', RunValue)
      and ((Pos(Lowercase(InstallRoot + '\jira_overlay.pyw'), Lowercase(RunValue)) > 0) or
           (Pos(Lowercase(InstallRoot + '\launch_installed.pyw'), Lowercase(RunValue)) > 0)) then
      WizardSelectTasks('startup')
    else WizardSelectTasks('!startup');
  end;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (PageID = LegacyPage.ID) and
    (AlreadyInstalled or HasFlag('NOMIGRATE'));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var Source, Detail: String;
begin
  Result := '';
  Source := LegacyPage.Values[0];
  if HasFlag('NOMIGRATE') or AlreadyInstalled then Source := '';
  PreparationPage.SetText('Checking the previous installation...', 'Running apps will be asked to quit automatically.');
  PreparationPage.SetProgress(0, 100);
  PreparationPage.Show;
  ShowingPreparation := True;
  try
    if not RunHelper('Prepare', Source, Helper, False) then begin
      if not ReadResult(Detail) then Detail := 'Preparation failed. No application code was replaced.';
      Result := Detail;
    end;
  finally
    ShowingPreparation := False;
    PreparationPage.Hide;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var Source, Detail: String;
begin
  if CurStep = ssPostInstall then begin
    Source := LegacyPage.Values[0];
    if HasFlag('NOMIGRATE') or AlreadyInstalled then Source := '';
    if not RunHelper('Finish', Source, Helper, WizardIsTaskSelected('startup') and not HasFlag('NOSTARTUP')) then begin
      if not ReadResult(Detail) then Detail := '[JIRA-I130] Startup registration failed.';
      RaiseException(Detail + ' Program files are installed. Run setup again to repair startup registration.');
    end;
  end;
end;

function InitializeUninstall: Boolean;
var Detail: String;
begin
  ErrorFile := ExpandConstant('{tmp}\jira-uninstall-result.txt');
  InstallRoot := ExpandConstant('{app}');
  Result := RunHelper('Stop', '', ExpandConstant('{app}\installer_support.ps1'), False);
  if not Result then begin
    if not ReadResult(Detail) then Detail := 'Close both Jira tray icons and retry uninstall.';
    MsgBox(Detail, mbError, MB_OK);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var Detail: String;
begin
  if CurUninstallStep = usUninstall then
    if not RunHelper('RemoveStartup', '', ExpandConstant('{app}\installer_support.ps1'), False) then begin
      if not ReadResult(Detail) then Detail := '[JIRA-I130] Cannot remove this installation startup entries.';
      RaiseException(Detail + ' Uninstall cancelled; personal data is retained.');
    end;
end;

// No broad uninstall deletion: Inno removes ONLY files it installed.
// Settings, credentials, models, history, reports and private backups remain.
