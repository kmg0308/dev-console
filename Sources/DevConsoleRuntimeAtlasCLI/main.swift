import Darwin
import RuntimeAtlasCommandLine

exit(runRuntimeAtlasCommandLine(arguments: Array(CommandLine.arguments.dropFirst())))
