// Compatibility bridge for pre-update-listener builds. Never terminates a
// process: sends the same window-loop messages used by Tk close / pystray stop.
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.RegularExpressions;

public static class JiraLegacyShutdown {
    private delegate bool EnumProc(IntPtr hwnd, IntPtr parameter);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumProc callback, IntPtr parameter);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetClassName(IntPtr hwnd, StringBuilder value, int capacity);
    [DllImport("user32.dll", SetLastError=true)] private static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr w, IntPtr l);
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, SetLastError=true)] private static extern IntPtr CommandLineToArgvW(string value, out int count);
    [DllImport("kernel32.dll")] private static extern IntPtr LocalFree(IntPtr address);

    public static string[] Arguments(string command) {
        int count;
        IntPtr address = CommandLineToArgvW(command, out count);
        if (address == IntPtr.Zero) throw new InvalidOperationException("Cannot inspect application command line.");
        try {
            var result = new string[count];
            for (int i = 0; i < count; i++) result[i] = Marshal.PtrToStringUni(Marshal.ReadIntPtr(address, i * IntPtr.Size));
            return result;
        } finally { LocalFree(address); }
    }

    public static int Request(uint pid, bool learner) {
        int sent = 0;
        EnumProc callback = delegate(IntPtr hwnd, IntPtr unused) {
            uint owner;
            GetWindowThreadProcessId(hwnd, out owner);
            if (owner != pid) return true;
            var name = new StringBuilder(256);
            GetClassName(hwnd, name, name.Capacity);
            bool match = learner ? Regex.IsMatch(name.ToString(), @"^TriageLearner[0-9]+SystemTrayIcon$")
                                 : name.ToString() == "TkTopLevel";
            // WM_USER+10 is pystray's own stop-loop message. WM_CLOSE is Tk's
            // normal close protocol; do not send either to arbitrary windows.
            if (match && PostMessage(hwnd, learner ? 0x040Au : 0x0010u, IntPtr.Zero, IntPtr.Zero)) sent++;
            return true;
        };
        if (!EnumWindows(callback, IntPtr.Zero)) throw new InvalidOperationException("Cannot inspect legacy application windows.");
        GC.KeepAlive(callback);
        return sent;
    }
}
