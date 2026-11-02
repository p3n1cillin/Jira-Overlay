// Isolated harness: no real startup entries, tasks or application clients.
using System;
public static class HelperTest {
    public static int Main(string[] args) {
        try {
            if (args[0] == "migrate") {
                JiraInstaller.ValidateTarget(args[2],"",true);
                JiraInstaller.SafeTree(args[1],false);
                JiraInstaller.Migrate(args[1],args[2]);
            } else if (args[0] == "validate") JiraInstaller.ValidateTarget(args[1],args.Length > 2 ? args[2] : "",true);
            else if (args[0] == "stop") JiraInstaller.StopCompanions(new[] {args[1]},new[] {args[2]},1);
            else if (args[0] == "ownership") {
                if (!JiraInstaller.OwnedCommand("\"pythonw\" \"" + args[1] + "\\launch_installed.pyw\"",args[1])) return 2;
                if (JiraInstaller.OwnedCommand("\"pythonw\" \"C:\\OtherApp\\launch_installed.pyw\"",args[1])) return 3;
            } else return 4;
            return 0;
        } catch(Exception ex) { Console.Error.WriteLine(ex.Message); return 1; }
    }
}
