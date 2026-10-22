// Build-time .NET Framework helper. No PowerShell, runtime compilation,
// remote-memory APIs, forced termination, task creation or task execution.
using System;
using System.IO;
using System.Text;
using System.Linq;
using System.Collections.Generic;
using System.Diagnostics;
using System.Threading;
using System.Security.Cryptography;
using System.Runtime.InteropServices;
using System.Text.RegularExpressions;
using Microsoft.Win32;

[assembly: System.Reflection.AssemblyTitle("Jira Overlay installer lifecycle")]
[assembly: System.Reflection.AssemblyDescription("Visible installer lifecycle and owned legacy-task cleanup")]
public static class JiraInstaller {
    const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    static string FailureCode = "JIRA-I100";
    static readonly string[] Names = {"JiraOverlay", "JiraTriageLearner"};
    static readonly string[] Scripts = {"jira_overlay.pyw", "triage_learner.pyw", "launch_installed.pyw"};
    static readonly string[] Mutexes = {"JiraOverlaySingleInstance", "JiraTriageLearner", "JiraOverlayGuardian"};
    static readonly string[] Personal = {"jira_config.json", "triage_state.json", "triage_suggestions.json", "triage_status.json", "triage_reviews.json", "triage_history.jsonl", "triage_dataset.jsonl", "triage_log.jsonl", "triage_log.jsonl.metrics.json", "triage_learner.log", "TRIAGE_REPORT.md", "TRIAGE_REVIEW_QUEUE.md"};
    public static string Canonical(string path) {
        if (String.IsNullOrWhiteSpace(path)) throw new ArgumentException("An application path is required.");
        return Path.GetFullPath(path).TrimEnd(Path.DirectorySeparatorChar);
    }
    static bool Same(string a, string b) { return String.Equals(a, b, StringComparison.OrdinalIgnoreCase); }
    static bool Exists(string path) { return File.Exists(path) || Directory.Exists(path); }
    public static void SafeTree(string path, bool recursive) {
        string current = Path.GetFullPath(path);
        while (!String.IsNullOrEmpty(current)) {
            if ((File.GetAttributes(current) & FileAttributes.ReparsePoint) != 0) throw new IOException("Linked/junction paths are not supported.");
            current = Path.GetDirectoryName(current);
        }
        if (recursive && Directory.Exists(path)) {
            foreach (string child in Directory.GetFileSystemEntries(path)) {
                if ((File.GetAttributes(child) & FileAttributes.ReparsePoint) != 0) throw new IOException("Linked files are not supported.");
                if (Directory.Exists(child)) SafeTree(child, true);
            }
        }
    }
    public static string[] PersonalFiles(string root) {
        if (!Directory.Exists(root)) return new string[0];
        return Directory.GetFiles(root).Where(p => Personal.Contains(Regex.Replace(Path.GetFileName(p), @"\.(bak|tmp|corrupt(?:[-.].*)?)$", ""), StringComparer.OrdinalIgnoreCase)).ToArray();
    }
    public static string Digest(string file) {
        using (var hash = SHA256.Create()) using (var stream = File.OpenRead(file))
            return BitConverter.ToString(hash.ComputeHash(stream)).Replace("-", "").ToLowerInvariant();
    }
    static string HashText(string text) {
        using (var hash = SHA256.Create()) return BitConverter.ToString(hash.ComputeHash(Encoding.UTF8.GetBytes(text))).Replace("-", "").ToLowerInvariant();
    }
    static void Progress(int percent, string message) { Console.WriteLine("JIRA_PROGRESS|{0}|{1}", percent, message); }
    public static bool Running(string[] names) {
        foreach (string name in names) {
            try { using (Mutex mutex = Mutex.OpenExisting(name)) return true; }
            catch (WaitHandleCannotBeOpenedException) { }
        }
        return false;
    }
    public static void StopCompanions(string[] roots, string[] names, int timeout) {
        FailureCode = "JIRA-I110";
        Progress(10, "Checking running applications...");
        if (!Running(names)) return;
        var handles = new List<EventWaitHandle>();
        try {
            foreach (string root in roots.Distinct(StringComparer.OrdinalIgnoreCase)) {
                string name = @"Local\JiraOverlay-" + HashText(Canonical(root).ToLowerInvariant()).Substring(0,24) + "-stop";
                try {
                    var handle = EventWaitHandle.OpenExisting(name, System.Security.AccessControl.EventWaitHandleRights.Modify);
                    handles.Add(handle);
                    handle.Set();
                } catch (WaitHandleCannotBeOpenedException) { }
            }
            FailureCode = "JIRA-I111";
            if (handles.Count == 0 && Running(names)) throw new IOException("Choose Quit on BOTH Jira tray icons, then Retry. Older builds have no safe shutdown listener. Nothing was force-killed.");
            var timer = Stopwatch.StartNew();
            int lastNotice = -5;
            while (Running(names)) {
                FailureCode = "JIRA-I112";
                if (timer.Elapsed.TotalSeconds >= timeout) throw new IOException("Apps are still saving or running. Choose Quit on BOTH Jira tray icons, wait, then Retry. Nothing was force-killed.");
                int elapsed = (int)timer.Elapsed.TotalSeconds;
                if (elapsed - lastNotice >= 5) { Progress(20, "Waiting for a safe save/close (" + elapsed + "s)..."); lastNotice = elapsed; }
                Thread.Sleep(200);
            }
        } finally { foreach (var handle in handles) handle.Dispose(); }
    }
    public static void Migrate(string oldRoot, string target) {
        FailureCode = "JIRA-I120";
        if (Same(oldRoot, target)) return;
        var files = PersonalFiles(oldRoot);
        foreach (string file in files) SafeTree(file, false);
        if (files.Length == 0) return;
        if (PersonalFiles(target).Length != 0) throw new IOException("Destination already has personal data. Refusing to merge two learners.");
        Directory.CreateDirectory(target);
        string stage = Path.Combine(target, ".migration-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(stage);
        var created = new List<string>();
        try {
            for (int i = 0; i < files.Length; i++) {
                Progress(35 + 55 * i / files.Length, "Copying and verifying personal data: " + Path.GetFileName(files[i]));
                string copy = Path.Combine(stage, Path.GetFileName(files[i]));
                File.Copy(files[i], copy, false);
                if (Digest(copy) != Digest(files[i])) throw new IOException("Personal-data copy failed verification. Originals retained.");
            }
            foreach (string file in files) {
                string destination = Path.Combine(target, Path.GetFileName(file));
                File.Move(Path.Combine(stage, Path.GetFileName(file)), destination);
                created.Add(destination);
            }
        } catch { foreach (string file in created) File.Delete(file); throw; }
        finally {
            if (!Same(Canonical(Path.GetDirectoryName(stage)), Canonical(target)) || !Regex.IsMatch(Path.GetFileName(stage), @"^\.migration-[a-f0-9]{32}$")) throw new IOException("Unsafe staging cleanup path.");
            SafeTree(stage, true);
            Directory.Delete(stage, true);
        }
    }
    public static bool OwnedCommand(string command, string root) {
        return command != null && Scripts.Any(script => command.IndexOf("\"" + Path.Combine(root, script) + "\"", StringComparison.OrdinalIgnoreCase) >= 0);
    }
    public static void RemoveStartup(string[] roots, bool keepPrimary = false) {
        using (var key = Registry.CurrentUser.OpenSubKey(RunKey, true)) {
            if (key == null) return;
            foreach (string name in Names) if (!(keepPrimary && name == "JiraOverlay") && roots.Any(root => OwnedCommand(key.GetValue(name) as string, root))) key.DeleteValue(name, false);
        }
    }
    public static string[] Discover() {
        var roots = new List<string>();
        using (var key = Registry.CurrentUser.OpenSubKey(RunKey)) {
            if (key == null) return roots.ToArray();
            foreach (string name in Names) {
                string value = key.GetValue(name) as string;
                if (value == null) continue;
                var match = Regex.Match(value, "\"([^\"\r\n]+\\\\(?:jira_overlay|triage_learner)\\.pyw)\"", RegexOptions.IgnoreCase);
                if (!match.Success) continue;
                string root = Canonical(Path.GetDirectoryName(match.Groups[1].Value));
                if (File.Exists(Path.Combine(root, Scripts[0])) && File.Exists(Path.Combine(root, Scripts[1]))) roots.Add(root);
            }
        }
        return roots.Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
    }
    // Migration only: delete the exact owned legacy task. Never register/run
    // a task, never enumerate arbitrary task names, retain its definition.
    public static void RetireTask(string root) {
        string record = Path.Combine(root,@".app-lifecycle\recovery.json");
        if (!File.Exists(record)) return; // New installs never contact Scheduler.
        if (new FileInfo(record).Length > 16384) throw new IOException("Oversized legacy recovery record.");
        string name = "JiraOverlay-" + HashText(root.ToLowerInvariant()).Substring(0,12) + "-Recovery";
        var recorded = Regex.Match(File.ReadAllText(record),"\"task\"\\s*:\\s*\"(JiraOverlay-[a-f0-9]{12}-Recovery)\"");
        if (recorded.Success) name = recorded.Groups[1].Value;
        dynamic service = Activator.CreateInstance(Type.GetTypeFromProgID("Schedule.Service"));
        service.Connect();
        dynamic folder = service.GetFolder("\\");
        dynamic task;
        try { task = folder.GetTask(name); }
        catch (COMException ex) { if ((uint)ex.ErrorCode == 0x80070002) return; throw; }
        dynamic definition = task.Definition;
        dynamic action = definition.Actions.Item(1);
        string arguments = (string)action.Arguments;
        bool script = arguments == "-B \"" + Path.Combine(root,"app_guard.pyw") + "\"" || arguments == "\"" + Path.Combine(root,"launch_installed.pyw") + "\"";
        if (definition.Actions.Count != 1 || !Same((string)action.Path, Path.Combine(root,@"runtime\pythonw.exe")) || !Same(((string)action.WorkingDirectory).TrimEnd('\\'),root) || !script)
            throw new IOException("Refusing to remove an unrelated scheduled task.");
        string logs = Path.Combine(root,".app-lifecycle");
        Directory.CreateDirectory(logs);
        SafeTree(logs,true);
        File.WriteAllText(Path.Combine(logs,"retired-task.xml"),(string)task.Xml, new UTF8Encoding(false));
        task.Enabled = false;
        folder.DeleteTask(name,0);
        Console.WriteLine("Owned legacy recovery task retired; definition retained locally.");
    }
    public static void ValidateTarget(string root, string version, bool prepare) {
        FailureCode = "JIRA-I101";
        if (Same(root,Canonical(Path.GetPathRoot(root))) || new[] {Environment.SpecialFolder.UserProfile,Environment.SpecialFolder.LocalApplicationData,Environment.SpecialFolder.Windows}.Any(p => Same(root,Canonical(Environment.GetFolderPath(p)))))
            throw new IOException("Choose a dedicated application folder, not a drive/profile/system root.");
        string ancestor = root;
        while (!Exists(ancestor)) ancestor = Path.GetDirectoryName(ancestor);
        SafeTree(ancestor, Same(ancestor,root));
        if (Directory.Exists(Path.Combine(root,".git")) || File.Exists(Path.Combine(root,".git")) || File.Exists(Path.Combine(root,@"app\app_release.py"))) throw new IOException("Never install over a Git/source checkout.");
        string release = Path.Combine(root,"app_release.py");
        if (prepare && version != "" && File.Exists(release)) {
            var match = Regex.Match(File.ReadAllText(release),"(?m)^VERSION\\s*=\\s*\"([0-9.]+)\"");
            if (match.Success && new Version(match.Groups[1].Value) > new Version(version)) throw new IOException("Newer build already installed. Downgrade refused.");
        }
        if (prepare && Directory.Exists(root) && Directory.GetFileSystemEntries(root).Length > 0 && !File.Exists(Path.Combine(root,".jira-overlay-install")) && !(File.Exists(Path.Combine(root,Scripts[0])) && File.Exists(Path.Combine(root,Scripts[1]))) && PersonalFiles(root).Length == 0)
            throw new IOException("Destination is a non-empty unrelated folder.");
    }
    public static int Main(string[] arguments) {
        string resultFile = "";
        try {
            var args = new Dictionary<string,string>(StringComparer.OrdinalIgnoreCase);
            for (int i = 0; i < arguments.Length; i++) {
                string key = arguments[i].TrimStart('-');
                if (args.ContainsKey(key)) throw new ArgumentException("Repeated option.");
                args[key] = key == "EnableStartup" ? "true" : arguments[++i];
            }
            resultFile = args.ContainsKey("ResultFile") ? args["ResultFile"] : "";
            string action = args["Action"];
            if (action == "Discover") { var found = Discover(); if (found.Length == 1 && resultFile != "") File.WriteAllText(resultFile,found[0],new UTF8Encoding(false)); return 0; }
            string root = Canonical(args["Target"]);
            ValidateTarget(root,args.ContainsKey("Version") ? args["Version"] : "",action == "Prepare");
            string source = args.ContainsKey("Source") ? Canonical(args["Source"]) : "";
            if (action == "Prepare" && source != "" && PersonalFiles(root).Length != 0) {
                Progress(5,"Existing destination learner retained. Separate ZIP import skipped; no datasets are merged.");
                source = "";
            }
            if (source != "") { SafeTree(source,false); if (!File.Exists(Path.Combine(source,Scripts[0])) || !File.Exists(Path.Combine(source,Scripts[1]))) throw new IOException("Select the old folder containing both application scripts."); }
            string[] roots = source == "" ? new[] {root} : new[] {root,source};
            if (action == "Prepare" || action == "Stop") {
                StopCompanions(roots,Mutexes,300);
                if (action == "Prepare" && source != "") Migrate(source,root);
            } else if (action == "Finish" || action == "RemoveStartup") {
                FailureCode = "JIRA-I130";
                RetireTask(root);
                bool enable = action == "Finish" && args.ContainsKey("EnableStartup");
                RemoveStartup(roots,enable);
                if (enable) {
                    string command = "\"" + Path.Combine(root,@"runtime\pythonw.exe") + "\" \"" + Path.Combine(root,"launch_installed.pyw") + "\"";
                    using (var key = Registry.CurrentUser.CreateSubKey(RunKey)) {
                        if (!Same(key.GetValue("JiraOverlay") as string,command)) key.SetValue("JiraOverlay",command,RegistryValueKind.String);
                    }
                }
            } else throw new ArgumentException("Unknown lifecycle action.");
            Progress(100,"Lifecycle operation complete. Personal data retained.");
            return 0;
        } catch (Exception ex) {
            string detail = "[" + FailureCode + "] " + ex.Message;
            if (resultFile != "") File.WriteAllText(resultFile,detail,new UTF8Encoding(false));
            Console.Error.WriteLine(detail);
            return 1;
        }
    }
}
