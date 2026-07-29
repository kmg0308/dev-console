import Darwin
import RuntimeAtlasSupervisorCore

exit(RuntimeAtlasSupervisorCore.run(arguments: Array(CommandLine.arguments.dropFirst())))
