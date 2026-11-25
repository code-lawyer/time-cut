using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class TimecutShellRead {
    [ComImport,Guid("43826d1e-e718-42ee-bc55-a1e261c37bfe"),InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItem {
        void BindToHandler(IntPtr context, ref Guid handler, ref Guid iid, out IStream stream);
    }
    [DllImport("shell32.dll",PreserveSig=false)]
    static extern void SHGetItemFromObject([MarshalAs(UnmanagedType.IUnknown)] object value,ref Guid iid,out IShellItem item);
    public static long Copy(object value,string destination,long length,bool boundedUnknown=false) {
        Guid itemId=typeof(IShellItem).GUID, streamId=typeof(IStream).GUID;
        Guid handler=new Guid("1cebb3ab-7c10-499a-a417-92ca16c4cb83");
        IShellItem item;
        try { SHGetItemFromObject(value,ref itemId,out item); }
        catch(COMException ex){ throw new COMException("shellItem",ex.HResult); }
        IStream stream=null;
        IntPtr count=Marshal.AllocCoTaskMem(sizeof(int));
        try {
            try { item.BindToHandler(IntPtr.Zero,ref handler,ref streamId,out stream); }
            catch(COMException ex){ throw new COMException("bindStream",ex.HResult); }
            using(var file=new FileStream(destination,FileMode.CreateNew,FileAccess.Write,FileShare.None)){
                byte[] buffer=new byte[65536];
                long written=0;
                while(written<length){
                    int requested=(int)Math.Min(buffer.Length,length-written);
                    stream.Read(buffer,requested,count);
                    int n=Marshal.ReadInt32(count);
                    if(n==0 && boundedUnknown) return written;
                    if(n<=0 || n>requested) throw new InvalidDataException();
                    file.Write(buffer,0,n);
                    written+=n;
                }
                stream.Read(buffer,1,count);
                if(Marshal.ReadInt32(count)!=0) throw new InvalidDataException();
                return written;
            }
        } finally {
            Marshal.FreeCoTaskMem(count);
            if(stream!=null) Marshal.ReleaseComObject(stream);
            Marshal.ReleaseComObject(item);
        }
    }
}
