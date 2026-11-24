//! Window-owned WKWebViews; every asynchronous operation captures its exact view.
use super::*;
use crate::policy::Outcome;
struct Preview{host:Retained<AnyObject>,stage:Retained<AnyObject>,web:Retained<AnyObject>,_delegate:Retained<UiDelegate>,id:String,document:String,thread:String,surface:String,scope:String,profile:Option<String>,generation:u64,requested:String,frontend:String,status:String,error:Option<String>,hidden:bool,visual_width:f64,visual_height:f64,viewport_width:f64,viewport_height:f64,corner_radius:f64}
thread_local!{static PREVIEWS:RefCell<HashMap<String,Preview>>=RefCell::new(HashMap::new());static TARGET:RefCell<String>=RefCell::new(String::from("main"));}
pub(super) fn view()->Option<Retained<AnyObject>>{let window=TARGET.with(|target|target.borrow().clone());PREVIEWS.with(|previews|previews.borrow().get(&window).map(|preview|retain(&preview.web)))}
fn with_target<T>(window:&str,work:impl FnOnce()->T)->T{struct Restore(String);impl Drop for Restore{fn drop(&mut self){TARGET.with(|target|*target.borrow_mut()=self.0.clone());}}let restore=Restore(TARGET.with(|target|std::mem::replace(&mut *target.borrow_mut(),window.into())));let result=work();drop(restore);result}
pub(crate) fn dispose(window:&str){let previous=PREVIEWS.with(|previews|previews.borrow_mut().remove(window));if let Some(preview)=previous{HOLES.with(|holes|holes.borrow_mut().remove(&key(&preview.host)));unsafe{let _:()=msg_send![&preview.web,stopLoading];let _:()=msg_send![&preview.host,removeFromSuperview];}}}
pub(crate) fn hide_inactive(window:&str,document:&str,thread:Option<&str>){PREVIEWS.with(|previews|{let mut previews=previews.borrow_mut();if let Some(preview)=previews.get_mut(window){if preview.document!=document{drop(previews);dispose(window);return}let hidden=Some(preview.thread.as_str())!=thread;preview.hidden=hidden;unsafe{let _:()=msg_send![&preview.host,setHidden:hidden];}}});}
pub(crate) fn state(window:&str)->Outcome{PREVIEWS.with(|previews|{let previews=previews.borrow();let preview=previews.get(window).ok_or_else(||BridgeError::new("desktop.browser_preview_missing","右侧浏览器当前不可见。"))?;unsafe{let title:Option<Retained<NSString>>=msg_send![&preview.web,title];let loading:bool=msg_send![&preview.web,isLoading];let back:bool=msg_send![&preview.web,canGoBack];let forward:bool=msg_send![&preview.web,canGoForward];let zoom:f64=msg_send![&preview.web,pageZoom];let zoom=if zoom.is_finite()&&zoom>0.{zoom}else{1.};Ok(json!({"id":preview.id,"url":url_text(msg_send![&preview.web,URL]),"title":title.map(|title|title.to_string()).unwrap_or_default(),"loading":loading,"can_go_back":back,"can_go_forward":forward,"canGoBack":back,"canGoForward":forward,"hidden":preview.hidden,"requestedUrl":preview.requested,"width":preview.viewport_width/zoom,"height":preview.viewport_height/zoom,"viewportWidth":preview.viewport_width,"viewportHeight":preview.viewport_height,"visualWidth":preview.visual_width,"visualHeight":preview.visual_height,"pageZoom":zoom,"threadId":preview.thread,"generation":preview.generation,"status":preview.status,"error":preview.error,"message":preview.error.as_deref().unwrap_or("")}))}})}
pub(super) fn changed(window:&str,web:&AnyObject,status:&str,error:Option<&AnyObject>){
    let owner=PREVIEWS.with(|previews|{let mut previews=previews.borrow_mut();let preview=previews.get_mut(window).filter(|preview|same(&preview.web,web))?;preview.status=status.into();if status=="loading"{preview.error=None;}else if status=="error"{preview.error=Some(error.map(script_error).unwrap_or_else(||"网页导航失败或被拒绝。".into()));}Some(preview.document.clone())});
    if let Some(document)=owner{if let Ok(value)=state(window){notify(Notice::WindowPreview(window.into(),document,value));}}
}
fn navigation_started(window:&str){PREVIEWS.with(|previews|{if let Some(preview)=previews.borrow_mut().get_mut(window){preview.status="loading".into();preview.error=None;}});}
fn apply_geometry(host:&AnyObject,stage:&AnyObject,web:&AnyObject,frame:NSRect,viewport:NSSize,corner_radius:f64){
    unsafe{
        let transaction=class(c"CATransaction");let _:()=msg_send![transaction,begin];let _:()=msg_send![transaction,setDisableActions:Bool::YES];
        let _:()=msg_send![host,setFrame:frame];
        let _:()=msg_send![host,setWantsLayer:Bool::YES];
        let can_clip:bool=msg_send![host,respondsToSelector:sel!(setClipsToBounds:)];
        if can_clip{let _:()=msg_send![host,setClipsToBounds:Bool::YES];}
        let layer:Option<Retained<AnyObject>>=msg_send![host,layer];
        if let Some(layer)=layer{
            let _:()=msg_send![&layer,setMasksToBounds:Bool::YES];
            let _:()=msg_send![&layer,setCornerRadius:corner_radius.max(0.)];
        }
        let stage_frame=NSRect::new(NSPoint::new(0.,0.),frame.size);
        let _:()=msg_send![stage,setFrame:stage_frame];
        let _:()=msg_send![stage,setBoundsOrigin:NSPoint::new(0.,0.)];
        let _:()=msg_send![stage,setBoundsSize:viewport];
        let web_frame=NSRect::new(NSPoint::new(0.,0.),viewport);
        let _:()=msg_send![web,setFrame:web_frame];
        let _:()=msg_send![transaction,commit];
    }
}
fn create(window:&str,document:&str,thread:&str,surface:&str,scope:&str,url:&str,profile:Option<String>,frame:NSRect,viewport:NSSize,corner_radius:f64,generation:u64)->Result<String,BridgeError>{
    let parent=workbench_web_view(window).ok_or_else(||BridgeError::new("desktop.window_closed","所属窗口已关闭。"))?;let mtm=MainThreadMarker::new().ok_or_else(||BridgeError::new("desktop.preview_thread_invalid","预览须在主线程创建。"))?;
    let id=uuid::Uuid::new_v4().to_string();unsafe{
        let config:Retained<AnyObject>=msg_send![class(c"WKWebViewConfiguration"),new];
        let store:Retained<AnyObject>=if let Some(profile)=&profile{
            let store_class=class(c"WKWebsiteDataStore");let available:bool=msg_send![store_class,respondsToSelector:sel!(dataStoreForIdentifier:)];if !available{return Err(BridgeError::new("desktop.preview_profile_unavailable","当前 macOS 未提供按身份隔离的持久浏览器存储。"));}
            let allocated:Allocated<AnyObject>=msg_send![class(c"NSUUID"),alloc];let uuid:Option<Retained<AnyObject>>=msg_send![allocated,initWithUUIDString:&*ns(profile)];let uuid=uuid.ok_or_else(||BridgeError::new("desktop.preview_profile_invalid","浏览器存储标识无效。"))?;msg_send![store_class,dataStoreForIdentifier:&*uuid]
        }else{msg_send![class(c"WKWebsiteDataStore"),nonPersistentDataStore]};
        let _:()=msg_send![&config,setWebsiteDataStore:&*store];let controller:Retained<AnyObject>=msg_send![&config,userContentController];let allocated:Allocated<AnyObject>=msg_send![class(c"WKUserScript"),alloc];let script:Retained<AnyObject>=msg_send![allocated,initWithSource:&*ns(include_str!("preview.js")),injectionTime:0isize,forMainFrameOnly:Bool::NO];let _:()=msg_send![&controller,addUserScript:&*script];
        if let Some(name)=safari_identity(){let _:()=msg_send![&config,setApplicationNameForUserAgent:&*ns(name)];}
        let host_alloc:Allocated<AnyObject>=msg_send![host_class(),alloc];let host:Retained<AnyObject>=msg_send![host_alloc,initWithFrame:frame];
        let _:()=msg_send![&host,setAutoresizesSubviews:Bool::NO];
        let stage_alloc:Allocated<AnyObject>=msg_send![scale_class(),alloc];let stage:Retained<AnyObject>=msg_send![stage_alloc,initWithFrame:NSRect::new(NSPoint::new(0.,0.),frame.size)];
        let _:()=msg_send![&stage,setAutoresizesSubviews:Bool::NO];
        let _:()=msg_send![&stage,setWantsLayer:Bool::YES];
        let allocated:Allocated<AnyObject>=msg_send![class(c"WKWebView"),alloc];let web:Retained<AnyObject>=msg_send![allocated,initWithFrame:NSRect::new(NSPoint::new(0.,0.),viewport),configuration:&*config];configure_web_view(&web);let delegate=UiDelegate::new(mtm,None,false,window.into());let _:()=msg_send![&web,setUIDelegate:&*delegate];let _:()=msg_send![&web,setNavigationDelegate:&*delegate];
        let _:()=msg_send![&stage,addSubview:&*web];
        let _:()=msg_send![&host,addSubview:&*stage];
        let _:()=msg_send![&parent,addSubview:&*host];
        apply_geometry(&host,&stage,&web,frame,viewport,corner_radius);
        PREVIEWS.with(|previews|previews.borrow_mut().insert(window.into(),Preview{host,stage,web,_delegate:delegate,id:id.clone(),document:document.into(),thread:thread.into(),surface:surface.into(),scope:scope.into(),profile,generation,requested:url.into(),frontend:url.into(),status:"loading".into(),error:None,hidden:false,visual_width:frame.size.width,visual_height:frame.size.height,viewport_width:viewport.width,viewport_height:viewport.height,corner_radius}));
    }with_target(window,||load(url))?;Ok(id)
}
pub(crate) fn open(window:&str,document:&str,p:&Value,scope:&str)->Outcome{
    let url=policy::string(p,"url").and_then(policy::web_url).ok_or_else(||BridgeError::new("desktop.preview_url_invalid","预览仅支持 HTTP/HTTPS 地址。"))?.to_string();policy::preview_url(&url,UI_ORIGIN.get().map_or("",String::as_str),crate::backend::backend_port())?;let thread=policy::string(p,"threadId").filter(|value|!value.is_empty()).ok_or_else(||BridgeError::new("desktop.preview_scope_invalid","预览须属于当前聊天。"))?;let surface=policy::string(p,"surfaceId").unwrap_or("");let rect=&p["rect"];
    let mut coordinates=[0.;4];for(index,key)in ["x","y","width","height"].iter().enumerate(){coordinates[index]=rect[*key].as_f64().filter(|value|value.is_finite()&&*value>=0.).ok_or_else(||BridgeError::new("desktop.preview_bounds_invalid","预览区域无效。"))?;}if coordinates[2]<=0.||coordinates[3]<=0.{return Err(BridgeError::new("desktop.preview_bounds_invalid","预览区域须大于零。"));}
    let viewport=if p["viewport"].is_null(){NSSize::new(coordinates[2],coordinates[3])}else{
        let vw=p["viewport"]["width"].as_f64().filter(|value|value.is_finite()&&(1.0..=3840.0).contains(value)).ok_or_else(||BridgeError::new("desktop.preview_bounds_invalid","预览区域无效。"))?;
        let vh=p["viewport"]["height"].as_f64().filter(|value|value.is_finite()&&(1.0..=3840.0).contains(value)).ok_or_else(||BridgeError::new("desktop.preview_bounds_invalid","预览区域无效。"))?;
        NSSize::new(vw,vh)
    };
    let corner_radius=p["cornerRadius"].as_f64().filter(|value|value.is_finite()&&*value>=0.).unwrap_or(0.);
    let parent=workbench_web_view(window).ok_or_else(||BridgeError::new("desktop.window_closed","所属窗口已关闭。"))?;let bounds:NSRect=unsafe{msg_send![&parent,bounds]};let flipped:bool=unsafe{msg_send![&parent,isFlipped]};let frame=NSRect::new(NSPoint::new(coordinates[0],if flipped{coordinates[1]}else{bounds.size.height-coordinates[1]-coordinates[3]}),NSSize::new(coordinates[2],coordinates[3]));
    let existing=PREVIEWS.with(|previews|{let mut previews=previews.borrow_mut();let preview=previews.get_mut(window)?;if preview.document!=document||preview.thread!=thread||preview.surface!=surface||preview.scope!=scope||preview.profile.is_some()!=(p["persistent"]==true){return None}
        let current=policy::web_url(&url_text(unsafe{msg_send![&preview.web,URL]})).map(|current|current.to_string());let navigate=preview.frontend!=url&&current.as_deref()!=Some(url.as_str());if preview.frontend!=url{preview.frontend=url.clone();preview.requested=url.clone();}
        preview.hidden=false;preview.visual_width=coordinates[2];preview.visual_height=coordinates[3];preview.viewport_width=viewport.width;preview.viewport_height=viewport.height;preview.corner_radius=corner_radius;
        apply_geometry(&preview.host,&preview.stage,&preview.web,frame,viewport,corner_radius);
        unsafe{let _:()=msg_send![&preview.host,setHidden:false];}set_holes(&preview.host,holes(p,&coordinates),corner_radius);Some((preview.id.clone(),navigate))});
    if let Some((id,navigate))=existing{if navigate{with_target(window,||load(&url))?;navigation_started(window);}else if p["reload"]==true{with_target(window,||navigation("reload"))?;}return Ok(json!({"id":id}))}
    let generation=PREVIEWS.with(|previews|previews.borrow().get(window).map_or(1,|preview|preview.generation+1));dispose(window);let profile=if p["persistent"]==true{Some(crate::bridge::preview_profile(&format!("{scope}:{window}"))?)}else{None};let id=create(window,document,thread,surface,scope,&url,profile,frame,viewport,corner_radius,generation)?;if let Some(host)=host_of(window){set_holes(&host,holes(p,&coordinates),corner_radius);}Ok(json!({"id":id}))
}
fn validate(window:&str,document:&str,method:&str,p:&Value)->Result<(),BridgeError>{PREVIEWS.with(|previews|{let previews=previews.borrow();let preview=previews.get(window).ok_or_else(||BridgeError::new("desktop.preview_changed","预览已改变。"))?;if preview.document!=document||p["id"].as_str().is_some_and(|id|id!=preview.id)||(preview.hidden&&method!="preview.state"&&!(method=="preview.navigate"&&preview.error.is_some())){return Err(BridgeError::new("desktop.preview_changed","预览已改变或隐藏。"));}Ok(())})}
fn navigation(action:&str)->Outcome{let web=preview_view()?;let started=unsafe{match action{"back"=>{let navigation:Option<Retained<AnyObject>>=msg_send![&web,goBack];navigation.is_some()},"forward"=>{let navigation:Option<Retained<AnyObject>>=msg_send![&web,goForward];navigation.is_some()},"reload"=>{let navigation:Option<Retained<AnyObject>>=msg_send![&web,reload];navigation.is_some()},"stop"=>{let _:()=msg_send![&web,stopLoading];false},_=>return Err(BridgeError::new("desktop.preview_action_invalid","预览操作无效。"))}};if started{let window=TARGET.with(|target|target.borrow().clone());navigation_started(&window);}Ok(Value::Null)}
pub(crate) fn invoke(window:&str,document:&str,method:&str,p:Value,reply:impl FnOnce(Outcome)+'static){
    if method=="preview.close"{let matched=PREVIEWS.with(|previews|previews.borrow().get(window).is_some_and(|preview|preview.document==document&&p["id"].as_str().is_none_or(|id|id==preview.id)&&p["surfaceId"].as_str().is_none_or(|surface|surface==preview.surface)));if matched{if p["hide"]==true{PREVIEWS.with(|previews|{if let Some(preview)=previews.borrow_mut().get_mut(window){preview.hidden=true;unsafe{let _:()=msg_send![&preview.host,setHidden:true];}}});}else{dispose(window)}}return reply(Ok(Value::Null))}
    if let Err(error)=validate(window,document,method,&p){return reply(Err(error))}
    let identity=PREVIEWS.with(|previews|{let previews=previews.borrow();let preview=&previews[window];(preview.id.clone(),preview.generation)});let reply_window=window.to_owned();let reply_document=document.to_owned();
    let reply=move|result|{let current=PREVIEWS.with(|previews|previews.borrow().get(&reply_window).is_some_and(|preview|preview.id==identity.0&&preview.generation==identity.1&&preview.document==reply_document));reply(if current{result}else{Err(BridgeError::new("desktop.preview_changed","异步操作完成时预览已改变。"))});};
    let window=window.to_owned();with_target(&window,||{
        let result=match method{
            "preview.state"=>state(&window),
            "preview.exec"=>return exec(p["script"].as_str().unwrap_or(""),reply),
            "preview.navigate"=>match policy::string(&p,"url").and_then(policy::web_url){Some(url)=>load(url.as_str()).map(|_|{PREVIEWS.with(|previews|{if let Some(preview)=previews.borrow_mut().get_mut(&window){preview.requested=url.to_string();}});navigation_started(&window);Value::Null}),None=>Err(BridgeError::new("desktop.browser_url_invalid","只能打开 HTTP/HTTPS 地址。"))},
            "preview.input"=>match p["kind"].as_str(){Some("click")=>click(p["x"].as_f64().unwrap_or(f64::NAN),p["y"].as_f64().unwrap_or(f64::NAN)).map(|_|Value::Null),Some("key")=>press(p["key"].as_str().unwrap_or(""),&p["modifiers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>(),p["repeat"].as_u64().unwrap_or(1).clamp(1,50)as usize).map(|_|Value::Null),_=>Err(BridgeError::new("desktop.browser_action_invalid","浏览器输入无效。"))},
            "preview.action"=>match p["action"].as_str().unwrap_or(""){
                action@("back"|"forward"|"reload"|"stop")=>navigation(action),"reload-hard"=>reload_from_origin().map(|_|{navigation_started(&window);Value::Null}),"devtools"=>open_preview_devtools(),
                "zoom"=>match p["factor"].as_f64().filter(|value|value.is_finite()&&(0.25..=3.).contains(value)){Some(value)=>set_zoom(value).map(|_|json!({"factor":value})),None=>Err(BridgeError::new("desktop.preview_zoom_invalid","缩放范围为 25% 至 300%。"))},
                "color-scheme"=>set_color_scheme(p["scheme"].as_str().unwrap_or("")).map(|_|Value::Null),
                "snapshot"=>{let rect=&p["rect"];let rect=if rect.is_null(){None}else{match(rect["x"].as_f64(),rect["y"].as_f64(),rect["width"].as_f64(),rect["height"].as_f64()){(Some(x),Some(y),Some(w),Some(h))if [x,y,w,h].iter().all(|value|value.is_finite())&&x>=0.&&y>=0.&&w>0.&&h>0.=>Some((x,y,w,h)),_=>return reply(Err(BridgeError::new("desktop.preview_capture_invalid","截图区域无效。")))}};return snapshot(rect,reply)},
                "logs"=>{
                    let kind=p["kind"].as_str().unwrap_or("console").to_owned();
                    if !matches!(kind.as_str(),"console"|"network"){return reply(Err(BridgeError::new("desktop.browser_logs_invalid","日志类型无效。")))}
                    let name=if kind=="network"{"__mutekiNetwork"}else{"__mutekiConsole"};
                    let script=format!("window.{name} ? {{rows:window.{name}.rows.slice(),dropped:window.{name}.dropped,error:window.{name}.error}} : null");
                    let offset=p["offset"].as_u64().unwrap_or(0)as usize;let limit=p["limit"].as_u64().unwrap_or(100).clamp(1,1000)as usize;let failed_only=p["failed_only"]==true;
                    return exec(&script,move|result|reply(result.and_then(|value|{
                        if value.is_null(){return Err(BridgeError::new("desktop.browser_logs_unavailable","日志尚未就绪，请重新打开网页。"))}
                        if !value["error"].is_null(){return Err(BridgeError::new("desktop.browser_logs_capture_limit",value["error"]["message"].as_str().map(str::to_owned).unwrap_or_else(||value["error"].to_string())))}
                        let rows=value["rows"].as_array().into_iter().flatten().map(|row|if kind=="network"{policy::normalize_network_log(row)}else{policy::normalize_console_log(row)}).filter(|row|kind!="network"||!failed_only||row["ok"]!=true).collect::<Vec<_>>();
                        let mut page=policy::paginate_logs(&rows,offset,limit);page["kind"]=json!(kind);page["dropped"]=value["dropped"].clone();Ok(page)
                    })))
                },
                "clear-data"|"clear-http-auth"=>{let metadata=PREVIEWS.with(|previews|previews.borrow().get(&window).map(|preview|(preview.id.clone(),preview.generation,preview.profile.clone()))).unwrap();if p["action"]=="clear-data"&&metadata.2.is_none(){return reply(Err(BridgeError::new("desktop.preview_not_persistent","当前预览没有持久登录数据。")))}let request=p["__replyId"].clone();clear_preview_data(request,metadata.0,metadata.1,metadata.2,p["action"]=="clear-data",true);return;},
                _=>Err(BridgeError::new("desktop.preview_action_invalid","此预览操作不可用。")),
            },_=>Err(BridgeError::new("desktop.preview_action_invalid","预览调用无效。")),
        };reply(result)
    });
}
pub(crate) fn cleared(window:&str,document:&str,id:&str,generation:u64,profile:Option<&str>,cleared_website_data:bool,cleared_http_auth:bool)->Outcome{
    let info=PREVIEWS.with(|previews|{let previews=previews.borrow();let preview=previews.get(window).filter(|preview|preview.document==document&&preview.id==id&&preview.generation==generation&&preview.profile.as_deref()==profile).ok_or_else(||BridgeError::new("desktop.preview_changed","预览已改变。"))?;let frame:NSRect=unsafe{msg_send![&preview.host,frame]};Ok::<_,BridgeError>((preview.thread.clone(),preview.surface.clone(),preview.scope.clone(),preview.requested.clone(),frame,NSSize::new(preview.viewport_width,preview.viewport_height),preview.corner_radius))})?;
    dispose(window);let next_id=create(window,document,&info.0,&info.1,&info.2,&info.3,profile.map(str::to_owned),info.4,info.5,info.6,generation+1)?;Ok(json!({"id":next_id,"cleared":true,"clearedWebsiteData":cleared_website_data,"clearedHttpAuth":cleared_http_auth,"generation":generation+1}))
}

pub(super) fn focused_in(window:&str)->bool{PREVIEWS.with(|previews|previews.borrow().get(window).is_some_and(|preview|!preview.hidden&&unsafe{let window:Option<Retained<AnyObject>>=msg_send![&preview.web,window];let Some(window)=window else{return false};let responder:Option<Retained<AnyObject>>=msg_send![&window,firstResponder];let Some(responder)=responder else{return false};same(&responder,&preview.web)||{let responds:bool=msg_send![&responder,respondsToSelector:sel!(isDescendantOf:)];responds&&msg_send![&responder,isDescendantOf:&*preview.web]}}))}
/// Workbench overlays (menus, popovers, toasts) drawn over the preview, in
/// its points from the top-left corner. The view is masked there so they show
/// through, and passes their clicks down to the workbench page.
type Hole=(NSRect,f64);
thread_local!{static HOLES:RefCell<HashMap<usize,Vec<Hole>>>=RefCell::new(HashMap::new());static LAST_PRESS:Cell<f64>=const{Cell::new(-1.)};}
fn key(host:&AnyObject)->usize{host as *const AnyObject as usize}
fn host_of(window:&str)->Option<Retained<AnyObject>>{PREVIEWS.with(|previews|previews.borrow().get(window).map(|preview|retain(&preview.host)))}
fn holes(p:&Value,coordinates:&[f64;4])->Vec<Hole>{
    p["holes"].as_array().into_iter().flatten().filter_map(|hole|{
        let value=|name:&str|hole[name].as_f64().filter(|value|value.is_finite());
        let (x,y,width,height)=(value("x")?-coordinates[0],value("y")?-coordinates[1],value("width")?,value("height")?);
        let (left,top)=(x.max(0.),y.max(0.));let (right,bottom)=((x+width).min(coordinates[2]),(y+height).min(coordinates[3]));
        (right-left>=1.&&bottom-top>=1.).then(||(NSRect::new(NSPoint::new(left,top),NSSize::new(right-left,bottom-top)),value("radius").unwrap_or(0.).max(0.)))
    }).collect()
}
#[repr(C)]struct CGPath{_opaque:[u8;0]}
unsafe impl objc2::encode::RefEncode for CGPath{const ENCODING_REF:objc2::encode::Encoding=objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("CGPath",&[]));}
#[link(name="CoreGraphics",kind="framework")]
unsafe extern "C"{fn CGPathCreateMutable()->*mut CGPath;fn CGPathAddRect(path:*mut CGPath,transform:*const std::ffi::c_void,rect:NSRect);fn CGPathAddRoundedRect(path:*mut CGPath,transform:*const std::ffi::c_void,rect:NSRect,width:f64,height:f64);fn CGPathRelease(path:*mut CGPath);}
fn set_holes(host:&AnyObject,holes:Vec<Hole>,corner_radius:f64){
    // SAFETY: CALayer/CAShapeLayer properties and a CGPath this function owns, on the main thread.
    unsafe{
        let _:()=msg_send![host,setWantsLayer:Bool::YES];
        let layer:Option<Retained<AnyObject>>=msg_send![host,layer];
        if let Some(layer)=layer{
            let transaction=class(c"CATransaction");let _:()=msg_send![transaction,begin];let _:()=msg_send![transaction,setDisableActions:Bool::YES];
            if holes.is_empty(){let _:()=msg_send![&layer,setMask:std::ptr::null::<AnyObject>()];}else{
                let bounds:NSRect=msg_send![host,bounds];let flipped:bool=msg_send![host,isFlipped];
                let path=CGPathCreateMutable();
                let outer_radius=corner_radius.max(0.).min(bounds.size.width/2.-0.01).min(bounds.size.height/2.-0.01);
                if outer_radius>0.{CGPathAddRoundedRect(path,std::ptr::null(),bounds,outer_radius,outer_radius)}else{CGPathAddRect(path,std::ptr::null(),bounds)}
                for(rect,radius)in &holes{
                    let y=if flipped{rect.origin.y}else{bounds.size.height-rect.origin.y-rect.size.height};let rect=NSRect::new(NSPoint::new(rect.origin.x,y),rect.size);
                    let radius=radius.min(rect.size.width/2.-0.01).min(rect.size.height/2.-0.01);
                    if radius>0.{CGPathAddRoundedRect(path,std::ptr::null(),rect,radius,radius)}else{CGPathAddRect(path,std::ptr::null(),rect)}
                }
                let shape:Retained<AnyObject>=msg_send![class(c"CAShapeLayer"),layer];let _:()=msg_send![&shape,setFrame:bounds];let _:()=msg_send![&shape,setFillRule:&*ns("even-odd")];let _:()=msg_send![&shape,setPath:path];CGPathRelease(path);
                let _:()=msg_send![&layer,setMask:&*shape];
            }
            let _:()=msg_send![transaction,commit];
        }
    }
    HOLES.with(|map|{let mut map=map.borrow_mut();if holes.is_empty(){map.remove(&key(host));}else{map.insert(key(host),holes);}});
}
extern "C-unwind" fn is_flipped(_this:&AnyObject,_:Sel)->Bool{Bool::YES}
fn host_class()->&'static AnyClass{
    static CLASS:OnceLock<usize>=OnceLock::new();
    let class=*CLASS.get_or_init(||{
        let mut builder=objc2::runtime::ClassBuilder::new(c"MutekiPreviewHostView",class(c"NSView")).expect("MutekiPreviewHostView is defined once");
        // SAFETY: hitTest: takes an NSPoint and returns an NSView or nil.
        unsafe{
            builder.add_method(sel!(isFlipped),is_flipped as extern "C-unwind" fn(_,_)->_);
            builder.add_method(sel!(hitTest:),hit_test as extern "C-unwind" fn(_,_,_)->_);
        }
        builder.register() as *const AnyClass as usize
    });
    // SAFETY: a registered class lives for the process.
    unsafe{&*(class as *const AnyClass)}
}
fn scale_class()->&'static AnyClass{
    static CLASS:OnceLock<usize>=OnceLock::new();
    let class=*CLASS.get_or_init(||{
        let mut builder=objc2::runtime::ClassBuilder::new(c"MutekiPreviewScaleView",class(c"NSView")).expect("MutekiPreviewScaleView is defined once");
        unsafe{builder.add_method(sel!(isFlipped),is_flipped as extern "C-unwind" fn(_,_)->_);}
        builder.register() as *const AnyClass as usize
    });
    unsafe{&*(class as *const AnyClass)}
}
extern "C-unwind" fn hit_test(this:&AnyObject,_:Sel,point:NSPoint)->*mut AnyObject{
    // SAFETY: NSView geometry of a live view, then the superclass's hitTest:.
    unsafe{
        let hidden:bool=msg_send![this,isHidden];
        if hidden{return std::ptr::null_mut()}
        let superview:Option<Retained<AnyObject>>=msg_send![this,superview];
        if let Some(superview)=superview{
            let local:NSPoint=msg_send![this,convertPoint:point,fromView:&*superview];let flipped:bool=msg_send![this,isFlipped];let bounds:NSRect=msg_send![this,bounds];
            let y=if flipped{local.y}else{bounds.size.height-local.y};
            let covered=HOLES.with(|holes|holes.try_borrow().ok().is_some_and(|holes|holes.get(&key(this)).is_some_and(|rects|rects.iter().any(|(rect,_)|local.x>=rect.origin.x&&local.x<rect.origin.x+rect.size.width&&y>=rect.origin.y&&y<rect.origin.y+rect.size.height))));
            if covered{return std::ptr::null_mut()}
            if local.x>=0.&&y>=0.&&local.x<bounds.size.width&&y<bounds.size.height{pressed(this);}
        }
        msg_send![super(this,class(c"NSView")),hitTest:point]
    }
}
/// A press inside the page never reaches the workbench document, so its
/// outside-click handlers (open menus) hear about it from here.
fn pressed(host:&AnyObject){
    // SAFETY: NSApplication's current event.
    let stamp=unsafe{let app:Retained<AnyObject>=msg_send![class(c"NSApplication"),sharedApplication];let event:Option<Retained<AnyObject>>=msg_send![&app,currentEvent];event.and_then(|event|{let kind:usize=msg_send![&event,type];matches!(kind,1|3|25).then(||{let stamp:f64=msg_send![&event,timestamp];stamp})})};
    let Some(stamp)=stamp else{return};if LAST_PRESS.with(|last|last.replace(stamp))==stamp{return}
    let owner=PREVIEWS.with(|previews|previews.try_borrow().ok().and_then(|previews|previews.iter().find(|(_,preview)|same(&preview.host,host)).map(|(window,preview)|(window.clone(),preview.document.clone(),preview.id.clone(),preview.thread.clone()))));
    if let Some((window,document,id,thread))=owner{notify(Notice::WindowPreview(window,document,json!({"id":id,"threadId":thread,"status":"pointerdown"})));}
}
/// Safari's own product token, so sites see the WebKit they are talking to.
fn safari_identity()->Option<&'static str>{
    static NAME:OnceLock<Option<String>>=OnceLock::new();
    NAME.get_or_init(||{
        // SAFETY: NSBundle info dictionary lookups.
        let version=unsafe{let bundle:Option<Retained<AnyObject>>=msg_send![class(c"NSBundle"),bundleWithPath:&*ns("/Applications/Safari.app")];bundle.and_then(|bundle|{let info:Option<Retained<AnyObject>>=msg_send![&bundle,infoDictionary];info.and_then(|info|{let value:Option<Retained<AnyObject>>=msg_send![&info,objectForKey:&*ns("CFBundleShortVersionString")];value.filter(|value|msg_send![&**value,isKindOfClass:class(c"NSString")]).map(|value|(*(Retained::as_ptr(&value) as *const NSString)).to_string())})})};
        match version{Some(version)=>Some(format!("Version/{version} Safari/605.1.15")),None=>{eprintln!("Muteki: Safari.app has no version; the preview keeps WebKit's default user agent");None}}
    }).as_deref()
}
