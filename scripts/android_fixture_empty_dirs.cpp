// Windows QA helper. The production importer has no dependency on this program.
// Opening for write and Delete are reachable only after checking a binding made
// while the unpredictable fixture marker was still readable on the phone.
#include <windows.h>
#include <portabledeviceapi.h>
#include <portabledevice.h>
#include <propvarutil.h>
#include <wincrypt.h>
#include <wrl/client.h>
#include <algorithm>
#include <iostream>
#include <memory>
#include <string>
#include <vector>
using Microsoft::WRL::ComPtr;
using std::wstring;

namespace {
struct Failure { const char* message; HRESULT status; };
void require(bool good, const char* error) { if (!good) throw Failure{error,E_FAIL}; }
void ok(HRESULT hr, const char* label) { if (FAILED(hr)) throw Failure{label,hr}; }
struct FreeString { void operator()(wchar_t* p) const { CoTaskMemFree(p); } };
using CoString = std::unique_ptr<wchar_t,FreeString>;

template<class T> ComPtr<T> create(REFCLSID cls) {
    ComPtr<T> obj; ok(CoCreateInstance(cls,nullptr,CLSCTX_INPROC_SERVER,IID_PPV_ARGS(&obj)),"COM activation"); return obj;
}
wstring digest(const BYTE* bytes, DWORD n) {
    BYTE hash[32]; DWORD length=sizeof(hash);
    require(CryptHashCertificate2(L"SHA256",0,nullptr,bytes,n,hash,&length) && length==32,"SHA256");
    wstring out;
    for (const auto byte:hash) { constexpr auto digits=L"0123456789abcdef"; out+=digits[byte>>4]; out+=digits[byte&15]; }
    return out;
}
wstring key(const wstring& value) {
    require(!value.empty(),"missing persistent WPD identity");
    // Native binding hashes deliberately use UTF-16LE, independent of Shell namespace keys.
    return digest(reinterpret_cast<const BYTE*>(value.data()),static_cast<DWORD>(value.size()*sizeof(wchar_t)));
}
bool isHex(const wstring& value, size_t length) {
    return value.size()==length && value.find_first_not_of(L"0123456789abcdef")==wstring::npos;
}
wstring lower(wstring value) {
    for(auto& c:value) { if(c>=L'A' && c<=L'Z') c+=L'a'-L'A'; }
    return value;
}
wstring findDevice(const wstring& expected) {
    auto manager=create<IPortableDeviceManager>(CLSID_PortableDeviceManager);
    ok(manager->RefreshDeviceList(),"refresh WPD devices");
    DWORD count=0; ok(manager->GetDevices(nullptr,&count),"count WPD devices");
    require(count>0,"no WPD device");
    std::vector<LPWSTR> raw(count);
    HRESULT listed=manager->GetDevices(raw.data(),&count);
    std::vector<wstring> matches;
    for (auto p:raw) {
        CoString id(p);
        if (!id || FAILED(listed)) continue;
        auto canonical=lower(id.get());
        if (canonical.find(L"vid_04e8")!=wstring::npos && (expected.empty() || key(canonical)==expected)) matches.emplace_back(id.get());
    }
    ok(listed,"list WPD devices"); require(listed==S_OK,"incomplete WPD device list");
    require(matches.size()==1,"expected a single matching Samsung USB interface"); return matches[0];
}
wstring stringValue(IPortableDeviceValues* values, REFPROPERTYKEY prop, bool required=true) {
    PWSTR p=nullptr; HRESULT hr=values->GetStringValue(prop,&p); CoString value(p);
    if (!required && (FAILED(hr) || !value || !value.get()[0])) return {};
    ok(hr,"read WPD string"); require(value && value.get()[0],"empty WPD string"); return value.get();
}
class Phone {
    ComPtr<IPortableDevice> device;
    ComPtr<IPortableDeviceProperties> properties;
    ComPtr<IPortableDeviceKeyCollection> keys;
public:
    ComPtr<IPortableDeviceContent> content;
    Phone(const wstring& id, bool writable) {
        device=create<IPortableDevice>(CLSID_PortableDeviceFTM);
        auto client=create<IPortableDeviceValues>(CLSID_PortableDeviceValues);
        ok(client->SetStringValue(WPD_CLIENT_NAME,L"Time-cut controlled fixture QA"),"set client");
        ok(client->SetUnsignedIntegerValue(WPD_CLIENT_DESIRED_ACCESS,writable?GENERIC_READ|GENERIC_WRITE:GENERIC_READ),"set access");
        ok(device->Open(id.c_str(),client.Get()),"open selected WPD device");
        ok(device->Content(&content),"content"); ok(content->Properties(&properties),"properties");
        keys=create<IPortableDeviceKeyCollection>(CLSID_PortableDeviceKeyCollection);
        for(auto k:{WPD_OBJECT_NAME,WPD_OBJECT_ORIGINAL_FILE_NAME,WPD_OBJECT_CONTENT_TYPE,WPD_OBJECT_PARENT_ID,
                    WPD_OBJECT_PERSISTENT_UNIQUE_ID,WPD_FUNCTIONAL_OBJECT_CATEGORY,WPD_DEVICE_SERIAL_NUMBER}) ok(keys->Add(k),"request property");
    }
    ComPtr<IPortableDeviceValues> values(const wstring& id) const {
        ComPtr<IPortableDeviceValues> v; ok(properties->GetValues(id.c_str(),keys.Get(),&v),"read object properties"); return v;
    }
    std::vector<wstring> children(const wstring& parent) const {
        ComPtr<IEnumPortableDeviceObjectIDs> it; ok(content->EnumObjects(0,parent.c_str(),nullptr,&it),"enumerate direct children");
        std::vector<wstring> ids;
        for(;;) {
            PWSTR p=nullptr; DWORD n=0; HRESULT hr=it->Next(1,&p,&n); CoString value(p);
            ok(hr,"enumeration failed");
            if(n) { require(value && value.get()[0],"unnamed object id"); ids.emplace_back(value.get()); }
            if(hr==S_FALSE) break;
            require(n==1,"incomplete enumeration");
        }
        return ids;
    }
    wstring named(const wstring& parent, const wstring& name, bool folder) const {
        wstring match;
        for(const auto& id:children(parent)) {
            auto v=values(id); auto n=stringValue(v.Get(),WPD_OBJECT_ORIGINAL_FILE_NAME,false);
            if(n.empty()) n=stringValue(v.Get(),WPD_OBJECT_NAME);
            if(lower(n)!=lower(name)) continue;
            GUID type{}; ok(v->GetGuidValue(WPD_OBJECT_CONTENT_TYPE,&type),"content type");
            require(n==name && match.empty() && (type==WPD_CONTENT_TYPE_FOLDER)==folder &&
                    stringValue(v.Get(),WPD_OBJECT_PARENT_ID)==parent,"ambiguous name, type or parent");
            match=id;
        }
        return match;
    }
    wstring persistent(const wstring& id) const { return key(stringValue(values(id).Get(),WPD_OBJECT_PERSISTENT_UNIQUE_ID)); }
    wstring marker(const wstring& id) const {
        ComPtr<IPortableDeviceResources> resources; ok(content->Transfer(&resources),"resources");
        DWORD buffer=0; ComPtr<IStream> stream;
        ok(resources->GetStream(id.c_str(),WPD_RESOURCE_DEFAULT,STGM_READ,&buffer,&stream),"open QA marker read-only");
        BYTE bytes[257]; ULONG total=0;
        while(total<sizeof(bytes)) {
            ULONG got=0; auto hr=stream->Read(bytes+total,static_cast<ULONG>(sizeof(bytes)-total),&got);
            ok(hr,"read QA marker"); total+=got; if(!got || hr==S_FALSE) break;
        }
        require(total>0 && total<sizeof(bytes),"unexpected marker size"); return digest(bytes,total);
    }
    void removeEmpty(const wstring& parent, const wstring& id, const wstring& name, const wstring& expected) const {
        require(named(parent,name,true)==id && persistent(id)==expected && children(id).empty(),"folder no longer bound or empty");
        auto selection=create<IPortableDevicePropVariantCollection>(CLSID_PortableDevicePropVariantCollection);
        PROPVARIANT v{}; ok(InitPropVariantFromString(id.c_str(),&v),"prepare one object id");
        HRESULT add=selection->Add(&v); PropVariantClear(&v); ok(add,"select one object");
        ComPtr<IPortableDevicePropVariantCollection> results;
        HRESULT hr=content->Delete(PORTABLE_DEVICE_DELETE_NO_RECURSION,selection.Get(),&results);
        ok(hr,"non-recursive delete refused"); require(results.Get()!=nullptr,"missing per-object delete results");
        DWORD count=0; ok(results->GetCount(&count),"result count"); require(count==1,"unexpected result count");
        PROPVARIANT result{}; ok(results->GetAt(0,&result),"read per-object result");
        const auto itemStatus=result.vt==VT_ERROR ? result.scode : E_UNEXPECTED;
        PropVariantClear(&result); ok(itemStatus,"per-object non-recursive delete refused");
        require(hr==S_OK && itemStatus==S_OK,"partial delete result");
        require(named(parent,name,true).empty(),"deleted object remains listed");
    }
};
void run(int argc,wchar_t** argv) {
    require(argc==5 || argc==6 || argc==10,"invalid arguments");
    const bool bind=wstring(argv[1])==L"bind";
    require((bind && (argc==5 || argc==6)) || (wstring(argv[1])==L"remove" && argc==10),"invalid mode");
    const wstring folder=argv[2], leaf=argv[3], owner=argv[4];
    require(folder.rfind(L"TimecutQa_",0)==0 && isHex(folder.substr(10),32),"invalid generated GUID folder");
    require((leaf==L".nomedia" || leaf==L"TimecutQaOwner.txt") && isHex(owner,64),"invalid marker binding");
    if(bind && argc==6) require(leaf==L"TimecutQaOwner.txt" && isHex(argv[5],64),"invalid separate provider marker");
    if(!bind) for(int i=5;i<10;i++) require(isHex(argv[i],64),"invalid device/object binding");
    auto id=findDevice(bind?L"":argv[5]); Phone phone(id,!bind);
    auto physical=key(stringValue(phone.values(WPD_DEVICE_OBJECT_ID).Get(),WPD_DEVICE_SERIAL_NUMBER));
    require(bind || physical==argv[6],"physical device identity differs");
    wstring storage,root,sub;
    for(const auto& candidate:phone.children(WPD_DEVICE_OBJECT_ID)) {
        auto properties=phone.values(candidate); GUID type{};
        if(FAILED(properties->GetGuidValue(WPD_FUNCTIONAL_OBJECT_CATEGORY,&type)) || type!=WPD_FUNCTIONAL_CATEGORY_STORAGE) continue;
        if(!bind && phone.persistent(candidate)!=argv[7]) continue;
        const auto directory=phone.named(candidate,folder,true);
        if(directory.empty() && bind) continue;
        require(storage.empty(),"ambiguous storage"); storage=candidate; root=directory;
    }
    require(!storage.empty(),"cannot bind a unique storage");
    if(root.empty() && !bind) { std::wcout<<L"absent\n"; return; }
    sub=phone.named(root,L"nested",true);
    if(bind) {
        require(!sub.empty(),"expected fixture subfolder");
        auto marker=phone.named(root,leaf,false); require(!marker.empty() && phone.marker(marker)==owner,"original ownership marker mismatch");
        if(argc==6) {
            const auto provider=phone.named(root,L".nomedia",false);
            require(!provider.empty() && phone.marker(provider)==argv[5],"independent provider marker mismatch");
        }
        auto s=phone.persistent(storage), r=phone.persistent(root), n=phone.persistent(sub);
        require(s!=r && s!=n && r!=n,"persistent IDs are not distinct");
        std::wcout<<key(lower(id))<<L"\n"<<physical<<L"\n"<<s<<L"\n"<<r<<L"\n"<<n<<L"\n";
        return;
    }
    require(phone.persistent(root)==argv[8],"root was replaced");
    auto entries=phone.children(root);
    require(entries.empty() || (entries.size()==1 && entries[0]==sub),"root contains an unexpected object");
    if(!sub.empty()) phone.removeEmpty(root,sub,L"nested",argv[9]);
    phone.removeEmpty(storage,root,folder,argv[8]);
    std::wcout<<L"removed\n";
}
}
int wmain(int argc,wchar_t** argv) {
    if(FAILED(CoInitializeEx(nullptr,COINIT_MULTITHREADED))) return 2;
    int result=0;
    try { run(argc,argv); } catch(const Failure& e) { std::cerr<<e.message<<" (0x"<<std::hex<<unsigned(e.status)<<")\n"; result=1; }
    catch(...) { std::cerr<<"Unrecognized WPD failure\n"; result=1; }
    CoUninitialize(); return result;
}
