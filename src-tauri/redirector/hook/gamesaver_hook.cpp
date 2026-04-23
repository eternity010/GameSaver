#include <windows.h>
#include <tlhelp32.h>

#include <algorithm>
#include <filesystem>
#include <fstream>
#include <string>
#include <vector>

using CreateFileWFn = HANDLE(WINAPI*)(LPCWSTR, DWORD, DWORD, LPSECURITY_ATTRIBUTES, DWORD, DWORD, HANDLE);
static CreateFileWFn g_originalCreateFileW = nullptr;
static std::vector<std::wstring> g_prefixes;
static std::wstring g_redirectRoot;
static std::wstring g_logPath;

extern "C" __declspec(dllexport) HANDLE WINAPI HookedCreateFileW(
    LPCWSTR fileName,
    DWORD desiredAccess,
    DWORD shareMode,
    LPSECURITY_ATTRIBUTES securityAttributes,
    DWORD creationDisposition,
    DWORD flagsAndAttributes,
    HANDLE templateFile);

namespace {
std::wstring ToLower(std::wstring value) {
  std::transform(value.begin(), value.end(), value.begin(), [](wchar_t c) { return static_cast<wchar_t>(towlower(c)); });
  return value;
}

std::wstring JsonUnescape(const std::wstring& text) {
  std::wstring out;
  out.reserve(text.size());
  for (size_t i = 0; i < text.size(); ++i) {
    wchar_t ch = text[i];
    if (ch != L'\\' || i + 1 >= text.size()) {
      out.push_back(ch);
      continue;
    }
    wchar_t next = text[++i];
    switch (next) {
      case L'\\':
        out.push_back(L'\\');
        break;
      case L'"':
        out.push_back(L'"');
        break;
      case L'/':
        out.push_back(L'/');
        break;
      case L'b':
        out.push_back(L'\b');
        break;
      case L'f':
        out.push_back(L'\f');
        break;
      case L'n':
        out.push_back(L'\n');
        break;
      case L'r':
        out.push_back(L'\r');
        break;
      case L't':
        out.push_back(L'\t');
        break;
      default:
        out.push_back(next);
        break;
    }
  }
  return out;
}

std::wstring NormalizePath(const std::wstring& input) {
  std::wstring out = input;
  std::replace(out.begin(), out.end(), L'/', L'\\');
  return ToLower(out);
}

void AppendLog(const std::wstring& line) {
  if (g_logPath.empty()) return;
  std::wofstream file(g_logPath, std::ios::app);
  if (!file) return;
  file << line << L"\n";
}

std::wstring ReadFileText(const std::wstring& path) {
  std::wifstream file(path);
  if (!file) return L"";
  std::wstring content((std::istreambuf_iterator<wchar_t>(file)), std::istreambuf_iterator<wchar_t>());
  return content;
}

std::wstring ExtractJsonString(const std::wstring& text, const std::wstring& key) {
  std::wstring token = L"\"" + key + L"\"";
  size_t keyPos = text.find(token);
  if (keyPos == std::wstring::npos) return L"";
  size_t colon = text.find(L':', keyPos);
  if (colon == std::wstring::npos) return L"";
  size_t firstQuote = text.find(L'"', colon + 1);
  if (firstQuote == std::wstring::npos) return L"";
  size_t secondQuote = text.find(L'"', firstQuote + 1);
  if (secondQuote == std::wstring::npos || secondQuote <= firstQuote) return L"";
  return JsonUnescape(text.substr(firstQuote + 1, secondQuote - firstQuote - 1));
}

std::vector<std::wstring> ExtractJsonStringArray(const std::wstring& text, const std::wstring& key) {
  std::vector<std::wstring> output;
  std::wstring token = L"\"" + key + L"\"";
  size_t keyPos = text.find(token);
  if (keyPos == std::wstring::npos) return output;
  size_t bracketStart = text.find(L'[', keyPos);
  size_t bracketEnd = text.find(L']', bracketStart);
  if (bracketStart == std::wstring::npos || bracketEnd == std::wstring::npos || bracketEnd <= bracketStart) {
    return output;
  }
  std::wstring body = text.substr(bracketStart + 1, bracketEnd - bracketStart - 1);
  size_t current = 0;
  while (current < body.size()) {
    size_t q1 = body.find(L'"', current);
    if (q1 == std::wstring::npos) break;
    size_t q2 = body.find(L'"', q1 + 1);
    if (q2 == std::wstring::npos) break;
    output.push_back(JsonUnescape(body.substr(q1 + 1, q2 - q1 - 1)));
    current = q2 + 1;
  }
  return output;
}

void LoadConfig() {
  DWORD pid = GetCurrentProcessId();
  std::wstring configPath = std::filesystem::temp_directory_path().wstring() + L"\\gamesaver\\redirect_config_" +
                            std::to_wstring(pid) + L".json";
  std::wstring config = ReadFileText(configPath);
  if (config.empty()) return;

  g_redirectRoot = ExtractJsonString(config, L"redirectRoot");
  g_logPath = ExtractJsonString(config, L"logPath");
  auto prefixes = ExtractJsonStringArray(config, L"confirmedPaths");
  g_prefixes.clear();
  for (const auto& p : prefixes) {
    if (!p.empty()) g_prefixes.push_back(NormalizePath(p));
  }
  g_redirectRoot = NormalizePath(g_redirectRoot);
  if (!g_redirectRoot.empty()) {
    std::filesystem::create_directories(std::filesystem::path(g_redirectRoot));
  }
  AppendLog(L"[hook] config loaded");
  AppendLog(L"[hook] redirectRoot=" + g_redirectRoot);
  AppendLog(L"[hook] prefixCount=" + std::to_wstring(g_prefixes.size()));
}

bool ReplaceIatEntry(HMODULE module, const char* importModule, const char* funcName, void* replacement, void** original) {
  if (!module) return false;
  auto* base = reinterpret_cast<unsigned char*>(module);
  auto* dos = reinterpret_cast<IMAGE_DOS_HEADER*>(base);
  if (dos->e_magic != IMAGE_DOS_SIGNATURE) return false;
  auto* nt = reinterpret_cast<IMAGE_NT_HEADERS*>(base + dos->e_lfanew);
  if (nt->Signature != IMAGE_NT_SIGNATURE) return false;

  auto& importDir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
  if (importDir.VirtualAddress == 0) return false;
  auto* desc = reinterpret_cast<IMAGE_IMPORT_DESCRIPTOR*>(base + importDir.VirtualAddress);

  for (; desc->Name != 0; ++desc) {
    const char* modName = reinterpret_cast<const char*>(base + desc->Name);
    if (_stricmp(modName, importModule) != 0) continue;

    auto* thunk = reinterpret_cast<IMAGE_THUNK_DATA*>(base + desc->FirstThunk);
    auto* origThunk = reinterpret_cast<IMAGE_THUNK_DATA*>(base + desc->OriginalFirstThunk);
    for (; origThunk->u1.AddressOfData != 0; ++origThunk, ++thunk) {
      if (IMAGE_SNAP_BY_ORDINAL(origThunk->u1.Ordinal)) continue;
      auto* import = reinterpret_cast<IMAGE_IMPORT_BY_NAME*>(base + origThunk->u1.AddressOfData);
      if (strcmp(reinterpret_cast<const char*>(import->Name), funcName) != 0) continue;

      DWORD oldProtect = 0;
      if (!VirtualProtect(&thunk->u1.Function, sizeof(ULONGLONG), PAGE_EXECUTE_READWRITE, &oldProtect)) {
        return false;
      }
      *original = reinterpret_cast<void*>(thunk->u1.Function);
      thunk->u1.Function = reinterpret_cast<ULONGLONG>(replacement);
      VirtualProtect(&thunk->u1.Function, sizeof(ULONGLONG), oldProtect, &oldProtect);
      FlushInstructionCache(GetCurrentProcess(), &thunk->u1.Function, sizeof(ULONGLONG));
      return true;
    }
  }
  return false;
}

int HookCreateFileWInAllModules() {
  HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetCurrentProcessId());
  if (snapshot == INVALID_HANDLE_VALUE) return 0;

  MODULEENTRY32W entry{};
  entry.dwSize = sizeof(entry);
  int replacedCount = 0;
  if (Module32FirstW(snapshot, &entry)) {
    do {
      void* original = nullptr;
      bool replaced = ReplaceIatEntry(entry.hModule, "KERNEL32.dll", "CreateFileW",
                                      reinterpret_cast<void*>(&HookedCreateFileW), &original);
      if (!replaced) {
        replaced = ReplaceIatEntry(entry.hModule, "KERNELBASE.dll", "CreateFileW",
                                   reinterpret_cast<void*>(&HookedCreateFileW), &original);
      }
      if (replaced) {
        replacedCount++;
        if (!g_originalCreateFileW && original) {
          g_originalCreateFileW = reinterpret_cast<CreateFileWFn>(original);
        }
      }
    } while (Module32NextW(snapshot, &entry));
  }
  CloseHandle(snapshot);
  return replacedCount;
}
}  // namespace

extern "C" __declspec(dllexport) const wchar_t* __stdcall GameSaverHookVersion() {
  return L"createfilew-iat-v1";
}

extern "C" __declspec(dllexport) HANDLE WINAPI HookedCreateFileW(
    LPCWSTR fileName,
    DWORD desiredAccess,
    DWORD shareMode,
    LPSECURITY_ATTRIBUTES securityAttributes,
    DWORD creationDisposition,
    DWORD flagsAndAttributes,
    HANDLE templateFile) {
  if (!g_originalCreateFileW || !fileName || g_prefixes.empty() || g_redirectRoot.empty()) {
    return g_originalCreateFileW ? g_originalCreateFileW(fileName, desiredAccess, shareMode, securityAttributes,
                                                         creationDisposition, flagsAndAttributes, templateFile)
                                 : INVALID_HANDLE_VALUE;
  }

  std::wstring normalized = NormalizePath(fileName);
  for (const auto& prefix : g_prefixes) {
    if (prefix.empty()) continue;
    if (normalized.rfind(prefix, 0) == 0) {
      std::wstring suffix = normalized.substr(prefix.size());
      while (!suffix.empty() && (suffix.front() == L'\\' || suffix.front() == L'/')) suffix.erase(suffix.begin());
      std::wstring redirected = g_redirectRoot + L"\\" + suffix;
      std::filesystem::create_directories(std::filesystem::path(redirected).parent_path());
      AppendLog(L"[hook] redirected: " + normalized + L" -> " + redirected);
      return g_originalCreateFileW(redirected.c_str(), desiredAccess, shareMode, securityAttributes,
                                   creationDisposition, flagsAndAttributes, templateFile);
    }
  }
  return g_originalCreateFileW(fileName, desiredAccess, shareMode, securityAttributes, creationDisposition,
                               flagsAndAttributes, templateFile);
}

BOOL APIENTRY DllMain(HMODULE hModule, DWORD reason, LPVOID /*reserved*/) {
  if (reason == DLL_PROCESS_ATTACH) {
    DisableThreadLibraryCalls(hModule);
    LoadConfig();
    int replacedCount = HookCreateFileWInAllModules();
    if (replacedCount > 0 && g_originalCreateFileW) {
      AppendLog(L"[hook] CreateFileW hook installed on modules=" + std::to_wstring(replacedCount));
    } else {
      AppendLog(L"[hook] failed to hook CreateFileW in loaded modules");
    }
  }
  return TRUE;
}
