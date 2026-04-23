#include <windows.h>

#include <iostream>
#include <string>
#include <vector>

namespace {
std::wstring Utf8ToWide(const std::string& input) {
  if (input.empty()) return L"";
  int needed = MultiByteToWideChar(CP_UTF8, 0, input.c_str(), -1, nullptr, 0);
  if (needed <= 0) return L"";
  std::wstring output(static_cast<size_t>(needed), L'\0');
  MultiByteToWideChar(CP_UTF8, 0, input.c_str(), -1, output.data(), needed);
  if (!output.empty() && output.back() == L'\0') output.pop_back();
  return output;
}

bool ParseArgs(int argc, char** argv, DWORD& pid, std::wstring& dllPath, std::wstring& configPath) {
  for (int i = 1; i < argc; ++i) {
    std::string key = argv[i];
    if (key == "--pid" && i + 1 < argc) {
      pid = static_cast<DWORD>(std::stoul(argv[++i]));
    } else if (key == "--dll" && i + 1 < argc) {
      dllPath = Utf8ToWide(argv[++i]);
    } else if (key == "--config" && i + 1 < argc) {
      configPath = Utf8ToWide(argv[++i]);
    }
  }
  return pid != 0 && !dllPath.empty() && !configPath.empty();
}
}  // namespace

int main(int argc, char** argv) {
  DWORD pid = 0;
  std::wstring dllPath;
  std::wstring configPath;
  if (!ParseArgs(argc, argv, pid, dllPath, configPath)) {
    std::cerr << "usage: gamesaver-injector --pid <pid> --dll <dllPath> --config <configPath>\n";
    return 2;
  }

  if (GetFileAttributesW(dllPath.c_str()) == INVALID_FILE_ATTRIBUTES) {
    std::cerr << "dll not found\n";
    return 3;
  }
  if (GetFileAttributesW(configPath.c_str()) == INVALID_FILE_ATTRIBUTES) {
    std::cerr << "config not found\n";
    return 4;
  }

  HANDLE process = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION |
                                   PROCESS_VM_WRITE | PROCESS_VM_READ,
                               FALSE, pid);
  if (!process) {
    std::cerr << "OpenProcess failed: " << GetLastError() << "\n";
    return 5;
  }

  SIZE_T dllBytes = (dllPath.size() + 1) * sizeof(wchar_t);
  LPVOID remoteDllPath =
      VirtualAllocEx(process, nullptr, dllBytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
  if (!remoteDllPath) {
    std::cerr << "VirtualAllocEx failed: " << GetLastError() << "\n";
    CloseHandle(process);
    return 6;
  }

  if (!WriteProcessMemory(process, remoteDllPath, dllPath.c_str(), dllBytes, nullptr)) {
    std::cerr << "WriteProcessMemory failed: " << GetLastError() << "\n";
    VirtualFreeEx(process, remoteDllPath, 0, MEM_RELEASE);
    CloseHandle(process);
    return 7;
  }

  HMODULE kernel32 = GetModuleHandleW(L"kernel32.dll");
  if (!kernel32) {
    std::cerr << "GetModuleHandleW(kernel32) failed\n";
    VirtualFreeEx(process, remoteDllPath, 0, MEM_RELEASE);
    CloseHandle(process);
    return 8;
  }
  auto loadLibraryW = reinterpret_cast<LPTHREAD_START_ROUTINE>(GetProcAddress(kernel32, "LoadLibraryW"));
  if (!loadLibraryW) {
    std::cerr << "GetProcAddress(LoadLibraryW) failed\n";
    VirtualFreeEx(process, remoteDllPath, 0, MEM_RELEASE);
    CloseHandle(process);
    return 9;
  }

  HANDLE remoteThread = CreateRemoteThread(process, nullptr, 0, loadLibraryW, remoteDllPath, 0, nullptr);
  if (!remoteThread) {
    std::cerr << "CreateRemoteThread failed: " << GetLastError() << "\n";
    VirtualFreeEx(process, remoteDllPath, 0, MEM_RELEASE);
    CloseHandle(process);
    return 10;
  }

  WaitForSingleObject(remoteThread, 10000);
  DWORD exitCode = 0;
  GetExitCodeThread(remoteThread, &exitCode);

  CloseHandle(remoteThread);
  VirtualFreeEx(process, remoteDllPath, 0, MEM_RELEASE);
  CloseHandle(process);

  if (exitCode == 0) {
    std::cerr << "LoadLibraryW returned NULL\n";
    return 11;
  }
  std::cout << "injected pid=" << pid << "\n";
  return 0;
}
