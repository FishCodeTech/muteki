package main

import (
    "bufio"
    "encoding/json"
    "errors"
    "io"
    "os"
    "path/filepath"
    "strings"
    "syscall"
)

type kimiUsageCheckpoint struct { home, cwd, session string; offset int64 }

func usageFile(path string, limit int64) (*os.File, error) {
    f, err := os.OpenFile(path, os.O_RDONLY|syscall.O_NOFOLLOW, 0)
    if err != nil { return nil, err }
    stat, err := f.Stat()
    if err != nil || !stat.Mode().IsRegular() || stat.Size() > limit { f.Close(); return nil, errors.New("native_usage_file_invalid") }
    return f, nil
}

func (c *kimiUsageCheckpoint) wirePath() (string,error) {
    index, err := usageFile(filepath.Join(c.home,"session_index.jsonl"),32*1024*1024)
    if err != nil { return "", errors.New("native_usage_index_unavailable") }; defer index.Close()
    scanner:=bufio.NewScanner(index); scanner.Buffer(make([]byte,65536),2*1024*1024)
    var selected string
    for scanner.Scan() {
        var row struct { SessionID string `json:"sessionId"`; WorkDir string `json:"workDir"`; SessionDir string `json:"sessionDir"` }
        if json.Unmarshal(scanner.Bytes(),&row)!=nil { return "",errors.New("native_usage_index_invalid") }
        if row.SessionID!=c.session {continue}
        cwd,err:=filepath.EvalSymlinks(row.WorkDir);if err!=nil||cwd!=c.cwd{continue}
        selected=filepath.Join(row.SessionDir,"agents","main","wire.jsonl")
    }
    if scanner.Err()!=nil||selected==""{return "",errors.New("native_usage_session_missing")}
    canonical,err:=filepath.EvalSymlinks(selected);if err!=nil{return "",errors.New("native_usage_wire_unavailable")}
    root,err:=filepath.EvalSymlinks(c.home);if err!=nil{return "",errors.New("native_usage_home_unavailable")}
    relative,err:=filepath.Rel(root,canonical);if err!=nil||relative==".."||strings.HasPrefix(relative,".."+string(filepath.Separator)){return "",errors.New("native_usage_path_invalid")}
    return canonical,nil
}

func checkpointKimiUsage(spec *WorkerSpec, env map[string]string) *kimiUsageCheckpoint {
    if len(spec.Argv)==0||filepath.Base(spec.Argv[0])!="kimi"{return nil}
    session:="";for i,arg:=range spec.Argv{if arg=="--session"&&i+1<len(spec.Argv){session=spec.Argv[i+1];break}}
    if session==""{return nil}
    cwd,err:=filepath.EvalSymlinks(spec.Cwd);if err!=nil{return nil}
    home:=env["KIMI_CODE_HOME"];if home==""{home=filepath.Join(env["HOME"],".kimi-code")}
    checkpoint:=&kimiUsageCheckpoint{home:home,cwd:cwd,session:session}
    if path,err:=checkpoint.wirePath();err==nil{if f,err:=usageFile(path,1024*1024*1024);err==nil{if stat,err:=f.Stat();err==nil{checkpoint.offset=stat.Size()};f.Close()}}
    return checkpoint
}

func (c *kimiUsageCheckpoint) frame() Frame {
    failure:=func(code string)Frame{return Frame{T:"err",Line:"Kimi native usage unavailable: "+code}}
    path,err:=c.wirePath();if err!=nil{return failure(err.Error())}
    file,err:=usageFile(path,1024*1024*1024);if err!=nil{return failure("native_usage_wire_invalid")};defer file.Close()
    stat,err:=file.Stat();if err!=nil||stat.Size()<c.offset||stat.Size()-c.offset>64*1024*1024{return failure("native_usage_append_invalid")}
    end:=stat.Size();if _,err=file.Seek(c.offset,io.SeekStart);err!=nil{return failure("native_usage_seek_failed")}
    scanner:=bufio.NewScanner(io.LimitReader(file,end-c.offset));scanner.Buffer(make([]byte,65536),4*1024*1024)
    totals:=[4]uint64{};records:=0
    for scanner.Scan(){var row struct { Type string `json:"type"`; AgentID string `json:"agentId"`; Usage json.RawMessage `json:"usage"` };if json.Unmarshal(scanner.Bytes(),&row)!=nil{return failure("native_usage_record_invalid")};if row.Type!="usage.record"||row.AgentID!="main"{continue}
        var usage map[string]json.Number;if json.Unmarshal(row.Usage,&usage)!=nil{return failure("native_usage_record_invalid")}
        for i,key:=range []string{"inputOther","output","inputCacheRead","inputCacheCreation"}{value,ok:=usage[key];if !ok{return failure("native_usage_counter_missing")};n,err:=value.Int64();if err!=nil||n<0||^uint64(0)-totals[i]<uint64(n){return failure("native_usage_counter_invalid")};totals[i]+=uint64(n)};records++
    }
    if scanner.Err()!=nil||records==0{return failure("native_usage_counters_unavailable")}
    input:=totals[0]+totals[2];if input<totals[0]||^uint64(0)-input<totals[3]{return failure("native_usage_counter_overflow")};input+=totals[3]
    value:=map[string]any{"type":"muteki.native_usage","scope":"message","usage_id":[]any{c.session,c.offset,end},"usage":map[string]any{"input_tokens":input,"output_tokens":totals[1],"cache_read_tokens":totals[2],"cache_write_tokens":totals[3],"input_includes_cache":true}}
    bytes,err:=json.Marshal(value);if err!=nil{return failure("native_usage_encode_failed")};return Frame{T:"out",Line:string(bytes)}
}
