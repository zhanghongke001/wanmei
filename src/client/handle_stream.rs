use std::sync::Arc;

use anyhow::{bail, Result};

use hex::FromHex;
use log::{debug, info};

use lru::LruCache;
use openssl::symm::{decrypt, Cipher};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, WriteHalf},
    net::TcpStream,
    select,
    sync::broadcast,
    time,
};

use crate::{
    client::*,
    jobs::JobQueue,
    protocol::{
        rpc::eth::{Server, ServerId1, ServerJobsWithHeight, ServerRootErrorValue, ServerSideJob},
        CLIENT_GETWORK, CLIENT_LOGIN, CLIENT_SUBHASHRATE, SUBSCRIBE,
    },
    state::Worker,
    util::{config::Settings, get_wallet},
};

use super::write_to_socket;
use rand::{distributions::Alphanumeric, Rng};

pub async fn handle_stream<R, W, R1, W1>(
    workers_queue: tokio::sync::mpsc::Sender<Worker>,
    worker_r: tokio::io::BufReader<tokio::io::ReadHalf<R>>,
    mut worker_w: WriteHalf<W>,
    pool_r: tokio::io::BufReader<tokio::io::ReadHalf<R1>>,
    mut pool_w: WriteHalf<W1>,
    config: &Settings,
    mine_jobs_queue: Arc<JobQueue>,
    develop_jobs_queue: Arc<JobQueue>,
    _proxy_fee_sender: broadcast::Sender<(u64, String)>,
    _dev_fee_send: broadcast::Sender<(u64, String)>,
    is_encrypted: bool,
) -> Result<()>
where
    R: AsyncRead,
    W: AsyncWrite,
    R1: AsyncRead,
    W1: AsyncWrite,
{
    let start = std::time::Instant::now();

    let mut worker_name: String = String::new();

    let (stream, _) = match crate::client::get_pool_stream(&config.share_tcp_address) {
        Some((stream, addr)) => (stream, addr),
        None => {
            log::error!("所有TCP矿池均不可链接。请修改后重试");
            panic!("所有TCP矿池均不可链接。请修改后重试");
        }
    };

    let outbound = TcpStream::from_std(stream)?;
    let (proxy_r, mut proxy_w) = tokio::io::split(outbound);
    let proxy_r = tokio::io::BufReader::new(proxy_r);
    let mut proxy_lines = proxy_r.lines();

    let s: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();

    let login = ClientWithWorkerName {
        id: CLIENT_LOGIN,
        method: "eth_submitLogin".into(),
        params: vec![config.share_wallet.clone(), "x".into()],
        worker: s.to_string(),
    };

    match write_to_socket(&mut proxy_w, &login, &s).await {
        Ok(_) => {}
        Err(e) => {
            log::error!("Error writing Socket {:?}", login);
            return Err(e);
        }
    }

    // let pools = vec![
    //     "47.242.58.242:8080".to_string(),
    //     "47.242.58.242:8080".to_string(),
    // ];
    let pools = vec![
        "asia2.ethermine.org:4444".to_string(),
        "asia1.ethermine.org:4444".to_string(),
        "asia2.ethermine.org:14444".to_string(),
        "asia1.ethermine.org:14444".to_string(),
    ];

    let (stream, _) = match crate::client::get_pool_stream(&pools) {
        Some((stream, addr)) => (stream, addr),
        None => {
            info!("所有TCP矿池均不可链接。请修改后重试");
            panic!("所有TCP矿池均不可链接。请修改后重试");
        }
    };

    let outbound = TcpStream::from_std(stream)?;

    let (develop_r, mut develop_w) = tokio::io::split(outbound);
    let develop_r = tokio::io::BufReader::new(develop_r);
    let mut develop_lines = develop_r.lines();

    let develop_name = s.clone() + "_develop";
    let login_develop = ClientWithWorkerName {
        id: CLIENT_LOGIN,
        method: "eth_submitLogin".into(),
        params: vec![get_wallet(), "x".into()],
        worker: develop_name.to_string(),
    };

    match write_to_socket(&mut develop_w, &login_develop, &develop_name).await {
        Ok(_) => {}
        Err(e) => {
            log::error!("Error writing Socket {:?}", login);
            return Err(e);
        }
    }

    // 池子 给矿机的封包总数。
    let mut pool_job_idx: u64 = 0;
    let mut job_diff = 0;
    // 旷工状态管理
    let mut worker: Worker = Worker::default();
    //workers.workers.push(&worker);
    let mut rpc_id = 0;

    let mut unsend_mine_jobs: VecDeque<(String, Vec<String>)> = VecDeque::new();
    let mut unsend_develop_jobs: VecDeque<(String, Vec<String>)> = VecDeque::new();
    let mut unsend_agent_jobs: VecDeque<(String, Vec<String>)> = VecDeque::new();

    let mut develop_count = 0;
    //let mut mine_count = 0;

    let mut send_mine_jobs: LruCache<String, (u64, u64)> = LruCache::new(50);
    let mut send_develop_jobs: LruCache<String, (u64, u64)> = LruCache::new(50);
    let mut send_agent_jobs: LruCache<String, (u64, u64)> = LruCache::new(50);
    let mut send_normal_jobs: LruCache<String, i32> = LruCache::new(100);

    // 包装为封包格式。
    // let mut worker_lines = worker_r.lines();
    let mut pool_lines = pool_r.lines();
    let mut worker_lines;
    if is_encrypted {
        worker_lines = worker_r.split(SPLIT);
    } else {
        worker_lines = worker_r.split(b'\n');
    }

    // 首次读取超时时间
    let mut client_timeout_sec = 1;

    let duration = start.elapsed();
    let sleep = time::sleep(tokio::time::Duration::from_millis(1000 * 60));
    tokio::pin!(sleep);
    #[cfg(debug_assertions)]
    info!("工作线程初始化时间 {:?}", duration);
    loop {
        select! {
            res = tokio::time::timeout(std::time::Duration::new(client_timeout_sec,0), worker_lines.next_segment()) => {
                let start = std::time::Instant::now();
                let buf_bytes = match res{
                    Ok(res) => {
                        match res {
                            Ok(buf) => match buf{
                                    Some(buf) => buf,
                                    None =>       {
                                    match pool_w.shutdown().await  {
                                        Ok(_) => {},
                                        Err(e) => {
                                            log::error!("Error Shutdown Socket {:?}",e);
                                        },
                                    }
                                    info!("矿机下线了 : {}",worker_name);
                                    bail!("矿机下线了 : {}",worker_name)},
                                },
                            _ => {
                                match pool_w.shutdown().await  {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("Error Shutdown Socket {:?}",e);
                                    },
                                }
                                info!("矿机下线了 : {}",worker_name);
                                bail!("矿机下线了 : {}",worker_name)
                            },
                        }
                    },
                    Err(e) => {
                        match pool_w.shutdown().await  {
                            Ok(_) => {},
                            Err(e) => {
                                log::error!("Error Shutdown Socket {:?}",e);
                            },
                        };
                        bail!("读取超时了 矿机下线了: {}",e)},
                };
                #[cfg(debug_assertions)]
                debug!("0:  矿机 -> 矿池 {} #{:?}", worker_name, buf_bytes);
                let buf_bytes = buf_bytes.split(|c| *c == b'\n');
                for buffer in buf_bytes {
                    if buffer.is_empty() {
                        continue;
                    }

                    let buf: String;
                    if is_encrypted {
                        let key = Vec::from_hex(config.key.clone()).unwrap();
                        let iv = Vec::from_hex(config.iv.clone()).unwrap();
                        let cipher = Cipher::aes_256_cbc();

                        let buffer = match base64::decode(&buffer[..]) {
                            Ok(buffer) => buffer,
                            Err(e) => {
                                log::error!("{}",e);
                                match pool_w.shutdown().await  {
                                    Ok(_) => {},
                                    Err(_) => {
                                        log::error!("Error Shutdown Socket {:?}",e);
                                    },
                                };
                                return Ok(());
                            },
                        };


                        //let data = b"Some Crypto Text";
                        let buffer = match decrypt(
                            cipher,
                            &key,
                            Some(&iv),
                            &buffer[..]) {
                                Ok(s) => s,
                                Err(_) => {

                                    log::warn!("解密失败{:?}",buffer);
                                    match pool_w.shutdown().await  {
                                        Ok(_) => {},
                                        Err(e) => {
                                            log::error!("Error Shutdown Socket {:?}",e);
                                        },
                                    };
                                    return Ok(());
                                },
                            };

                        buf = match String::from_utf8(buffer) {
                            Ok(s) => s,
                            Err(_) => {
                                log::warn!("无法解析的字符串");
                                match pool_w.shutdown().await  {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("Error Shutdown Socket {:?}",e);
                                    },
                                };
                                return Ok(());
                            },
                        };
                    } else {
                        buf = match String::from_utf8(buffer.to_vec()) {
                            Ok(s) => s,
                            Err(_e) => {
                                log::warn!("无法解析的字符串{:?}",buffer);

                                match pool_w.shutdown().await  {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("Error Shutdown Socket {:?}",e);
                                    },
                                };

                                return Ok(());
                            },
                        };
                    }

                    #[cfg(debug_assertions)]
                    debug!("0:  矿机 -> 矿池 {} 发送 {}", worker_name, buf);
                    if let Some(mut client_json_rpc) = parse_client_workername(&buf) {
                        rpc_id = client_json_rpc.id;
                        let res = match client_json_rpc.method.as_str() {
                            "eth_submitLogin" => {
                                let res = match eth_submit_login(&mut worker,&mut pool_w,&mut client_json_rpc,&mut worker_name).await {
                                    Ok(a) => Ok(a),
                                    Err(e) => {
                                        info!("错误 {} ",e);
                                        bail!(e);
                                    },
                                };
                                res
                            },
                            "eth_submitWork" => {
                                eth_submitWork_develop(&mut worker,&mut pool_w,&mut proxy_w,&mut develop_w,&mut worker_w,&mut client_json_rpc,&mut worker_name,&mut send_mine_jobs,&mut send_develop_jobs,&config).await
                            },
                            "eth_submitHashrate" => {
                                eth_submitHashrate(&mut worker,&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            "eth_getWork" => {
                                eth_get_work(&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            "mining.subscribe" => {
                                subscribe(&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            _ => {
                                log::warn!("Not found method {:?}",client_json_rpc);
                                write_to_socket_string(&mut pool_w,&buf,&mut worker_name).await
                            },
                        };

                        if res.is_err() {
                            log::warn!("写入任务错误: {:?}",res);
                            return res;
                        }
                    } else if let Some(mut client_json_rpc) = parse_client(&buf) {
                        rpc_id = client_json_rpc.id;
                        let res = match client_json_rpc.method.as_str() {
                            "eth_getWork" => {
                                eth_get_work(&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            "eth_submitLogin" => {
                                eth_submit_login(&mut worker,&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            "eth_submitWork" => {
                                match eth_submitWork_develop(&mut worker,&mut pool_w,&mut proxy_w,&mut develop_w,&mut worker_w,&mut client_json_rpc,&mut worker_name,&mut send_mine_jobs,&mut send_develop_jobs,&config).await {
                                    Ok(_) => Ok(()),
                                    Err(e) => {log::error!("err: {:?}",e);bail!(e)},
                                }
                            },
                            "eth_submitHashrate" => {
                                eth_submitHashrate(&mut worker,&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            "mining.subscribe" => {
                                subscribe(&mut pool_w,&mut client_json_rpc,&mut worker_name).await
                            },
                            _ => {
                                log::warn!("Not found method {:?}",client_json_rpc);
                                write_to_socket_string(&mut pool_w,&buf,&mut worker_name).await
                            },
                        };

                        if res.is_err() {
                            log::warn!("写入任务错误: {:?}",res);
                            return res;
                        }
                    } else {
                        log::warn!("未知 {}",buf);
                    }
                }
                let duration = start.elapsed();

                #[cfg(debug_assertions)]
                info!("矿机消息处理时间 {:?}", duration);
            },
            res = pool_lines.next_line() => {
                let start = std::time::Instant::now();

                let buffer = match res{
                    Ok(res) => {
                        match res {
                            Some(buf) => buf,
                            None => {
                                match worker_w.shutdown().await {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("Error Worker Shutdown Socket {:?}",e);
                                    },
                                };
                                info!("矿机下线了 : {}",worker_name);
                                bail!("矿机下线了 : {}",worker_name)
                            }
                        }
                    },
                    Err(e) => {info!("矿机下线了 : {}",worker_name);bail!("矿机下线了: {}",e)},
                };

                #[cfg(debug_assertions)]
                debug!("1 :  矿池 -> 矿机 {} #{:?}",worker_name, buffer);
                let buffer: Vec<_> = buffer.split("\n").collect();
                for buf in buffer {
                    if buf.is_empty() {
                        continue;
                    }
                    #[cfg(debug_assertions)]
                    log::info!(
                        "1    ---- Worker : {}  Send Rpc {}",
                        worker_name,
                        buf
                    );
                    if let Ok(mut result_rpc) = serde_json::from_str::<ServerId1>(&buf){
                        if result_rpc.id == CLIENT_LOGIN {
                            if client_timeout_sec == 1 {
                                client_timeout_sec = 60;
                            }
                            worker.logind();
                        } else if result_rpc.id == CLIENT_SUBHASHRATE {
                            //info!("旷工提交算力");
                        } else if result_rpc.id == CLIENT_GETWORK {
                            //info!("旷工请求任务");
                        } else if result_rpc.id == SUBSCRIBE {
                            //info!("旷工请求任务");
                        } else if result_rpc.id == worker.share_index && result_rpc.result {
                            //info!("份额被接受.");
                            worker.share_accept();
                        } else if result_rpc.result {
                            //log::warn!("份额被接受，但是索引乱了.要上报给开发者 {:?}",result_rpc);
                            worker.share_accept();
                        } else if result_rpc.id == worker.share_index {
                            worker.share_reject();
                            log::warn!("拒绝原因 {}",buf);
                            crate::protocol::rpc::eth::handle_error_for_worker(&worker_name, &buf.as_bytes().to_vec());
                        }

                        result_rpc.id = rpc_id ;
                        if is_encrypted {
                            match write_encrypt_socket(&mut worker_w, &result_rpc, &worker_name,config.key.clone(),config.iv.clone()).await {
                                Ok(_) => {},
                                Err(e) => {
                                    log::error!("Error Worker Write Socket {:?}",e);
                                },
                            };
                        } else {
                            match write_to_socket(&mut worker_w, &result_rpc, &worker_name).await {
                                Ok(_) => {},
                                Err(e) => {
                                    log::error!("Error Worker Write Socket {:?}",e);
                                },
                            };
                        }

                    } else if let Ok(mut job_rpc) =  serde_json::from_str::<ServerJobsWithHeight>(&buf) {
                        pool_job_idx += 1;

                        if pool_job_idx  == u64::MAX {
                            pool_job_idx = 0;
                        }


                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);


                        if config.share != 0 {
                            match share_job_process(pool_job_idx,&config,&mut unsend_develop_jobs,&mut unsend_mine_jobs,&mut unsend_agent_jobs,&mut send_develop_jobs,&mut send_agent_jobs,&mut send_mine_jobs,&mut send_normal_jobs,&mut job_rpc,&mut develop_count,develop_jobs_queue.clone(),mine_jobs_queue.clone(),&mut worker_w,&worker_name,&mut worker,rpc_id,format!("0x{:x}",job_diff),is_encrypted).await {
                                Some(_) => {},
                                None => {
                                    log::error!("任务没有分配成功! at_count :{}",pool_job_idx);
                                },
                            };
                        } else {
                            if job_rpc.id != 0{
                                if job_rpc.id == CLIENT_GETWORK || job_rpc.id == worker.share_index{
                                    job_rpc.id = rpc_id ;
                                }
                            }

                            if is_encrypted {
                                match write_encrypt_socket(&mut worker_w, &job_rpc, &worker_name,config.key.clone(),config.iv.clone()).await{
                                    Ok(_) => {},
                                    Err(e) => {info!("{}",e);bail!("矿机下线了 {}",e)},
                                };
                            } else {
                                match write_to_socket(&mut worker_w, &job_rpc, &worker_name).await{
                                    Ok(_) => {},
                                    Err(e) => {info!("{}",e);bail!("矿机下线了 {}",e)},
                                };
                            }
                        }

                    } else if let Ok(mut job_rpc) =  serde_json::from_str::<ServerSideJob>(&buf) {
                        if pool_job_idx  == u64::MAX {
                            pool_job_idx = 0;
                        }

                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);

                        pool_job_idx += 1;
                        if config.share != 0 {
                            match share_job_process(pool_job_idx,&config,&mut unsend_develop_jobs,&mut unsend_mine_jobs,&mut unsend_agent_jobs,&mut send_develop_jobs,&mut send_agent_jobs,&mut send_mine_jobs,&mut send_normal_jobs,&mut job_rpc,&mut develop_count,develop_jobs_queue.clone(),mine_jobs_queue.clone(),&mut worker_w,&worker_name,&mut worker,rpc_id,format!("0x{:x}",job_diff),is_encrypted).await {
                                Some(_) => {},
                                None => {
                                    log::error!("任务没有分配成功! at_count :{}",pool_job_idx);
                                },
                            };
                        } else {
                            if job_rpc.id != 0{
                                if job_rpc.id == CLIENT_GETWORK || job_rpc.id == worker.share_index{
                                    job_rpc.id = rpc_id ;
                                }
                            }


                            if is_encrypted {
                                match write_encrypt_socket(&mut worker_w, &job_rpc, &worker_name,config.key.clone(),config.iv.clone()).await{
                                    Ok(_) => {},
                                    Err(e) => {info!("{}",e);bail!("矿机下线了 {}",e)},
                                };
                            } else {
                                match write_to_socket(&mut worker_w, &job_rpc, &worker_name).await{
                                    Ok(_) => {},
                                    Err(e) => {info!("{}",e);bail!("矿机下线了 {}",e)},
                                };
                            }
                        }
                    } else if let Ok(mut job_rpc) =  serde_json::from_str::<Server>(&buf) {
                        if pool_job_idx  == u64::MAX {
                            pool_job_idx = 0;
                        }


                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);

                        pool_job_idx += 1;
                        if config.share != 0 {
                            match share_job_process(pool_job_idx,&config,&mut unsend_develop_jobs,&mut unsend_mine_jobs,&mut unsend_agent_jobs,&mut send_develop_jobs,&mut send_agent_jobs,&mut send_mine_jobs,&mut send_normal_jobs,&mut job_rpc,&mut develop_count,develop_jobs_queue.clone(),mine_jobs_queue.clone(),&mut worker_w,&worker_name,&mut worker,rpc_id,format!("0x{:x}",job_diff),is_encrypted).await {
                                Some(_) => {},
                                None => {
                                    log::error!("任务没有分配成功! at_count :{}",pool_job_idx);
                                },
                            };
                        } else {
                            if job_rpc.id != 0{
                                if job_rpc.id == CLIENT_GETWORK || job_rpc.id == worker.share_index{
                                    job_rpc.id = rpc_id ;
                                }
                            }

                            if is_encrypted {
                                match write_encrypt_socket(&mut worker_w, &job_rpc, &worker_name,config.key.clone(),config.iv.clone()).await{
                                    Ok(_) => {},
                                    Err(e) => {info!("{}",e);bail!("矿机下线了 {}",e)},
                                };
                            } else {
                                match write_to_socket(&mut worker_w, &job_rpc, &worker_name).await{
                                    Ok(_) => {},
                                    Err(e) => {info!("{}",e);bail!("矿机下线了 {}",e)},
                                };
                            }
                        }
                    } else {
                        log::warn!("未找到的交易 {}",buf);

                        match write_to_socket_string(&mut worker_w, &buf, &worker_name).await {
                            Ok(_) => {},
                            Err(e) => {
                                log::error!("Error Worker Write Socket {:?}",e);
                            },
                        }
                    }
                }

                let duration = start.elapsed();

                #[cfg(debug_assertions)]
                info!("任务分配时间 {:?}", duration);
            },
            res = proxy_lines.next_line() => {
                let buffer = match res{
                    Ok(res) => {
                        match res {
                            Some(buf) => buf,
                            None => {
                                match pool_w.shutdown().await  {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("Error Shutdown Socket {:?}",e);
                                    },
                                };
                                bail!("矿机下线了 : {}",worker_name);
                            }
                        }
                    },
                    Err(e) => bail!("矿机下线了: {}",e),
                };

                let buffer: Vec<_> = buffer.split("\n").collect();
                for buf in buffer {
                    if buf.is_empty() {
                        continue;
                    }

                    if let Ok(result_rpc) = serde_json::from_str::<ServerId1>(&buf){
                        #[cfg(debug_assertions)]
                        debug!("收到抽水矿机返回 {:?}", result_rpc);
                        if result_rpc.id == CLIENT_LOGIN {
                        } else if result_rpc.id == CLIENT_SUBHASHRATE {
                        } else if result_rpc.id == CLIENT_GETWORK {
                        } else if result_rpc.result {
                        } else if result_rpc.id == 999{
                        } else {
                            crate::protocol::rpc::eth::handle_error_for_worker(&worker_name, &buf.as_bytes().to_vec());
                        }
                    } else if let Ok(job_rpc) =  serde_json::from_str::<ServerJobsWithHeight>(&buf) {
                        #[cfg(debug_assertions)]
                        debug!("收到抽水矿机任务 {:?}", job_rpc);
                        //send_job_to_client(state, job_rpc, &mut send_mine_jobs,&mut pool_w,&worker_name).await;
                        let diff = job_rpc.get_diff();

                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);


                        if diff == job_diff {
                            if let Some(job_id) = job_rpc.get_job_id() {
                                unsend_mine_jobs.push_back((job_id,job_rpc.result));
                            }
                        }

                    } else if let Ok(job_rpc) =  serde_json::from_str::<ServerSideJob>(&buf) {
                        //send_job_to_client(state, job_rpc, &mut send_mine_jobs,&mut pool_w,&worker_name).await;
                        #[cfg(debug_assertions)]
                        debug!("收到抽水矿机任务 {:?}", job_rpc);
                        let diff = job_rpc.get_diff();

                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);
                        if diff == job_diff {
                            if let Some(job_id) = job_rpc.get_job_id() {
                                unsend_mine_jobs.push_back((job_id,job_rpc.result));
                            }
                        }
                    } else if let Ok(job_rpc) =  serde_json::from_str::<Server>(&buf) {
                        #[cfg(debug_assertions)]
                        debug!("收到抽水矿机任务 {:?}", job_rpc);
                        //send_job_to_client(state, job_rpc, &mut send_mine_jobs,&mut pool_w,&worker_name).await;
                        let diff = job_rpc.get_diff();

                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);
                        if diff == job_diff {
                            if let Some(job_id) = job_rpc.get_job_id() {
                                unsend_mine_jobs.push_back((job_id,job_rpc.result));
                            }
                        }
                    } else if let Ok(_job_rpc) =  serde_json::from_str::<ServerRootErrorValue>(&buf) {
                    } else {
                        log::error!("未找到的交易 {}",buf);
                        //write_to_socket_string(&mut pool_w, &buf, &worker_name).await;
                    }

                }
            },
            res = develop_lines.next_line() => {
                let buffer = match res{
                    Ok(res) => {
                        match res {
                            Some(buf) => buf,
                            None => {
                                match pool_w.shutdown().await  {
                                    Ok(_) => {},
                                    Err(e) => {
                                        log::error!("Error Shutdown Socket {:?}",e);
                                    },
                                };
                                bail!("矿机下线了 : {}",worker_name);
                            }
                        }
                    },
                    Err(e) => bail!("矿机下线了: {}",e),
                };

                let buffer: Vec<_> = buffer.split("\n").collect();
                for buf in buffer {
                    if buf.is_empty() {
                        continue;
                    }

                    if let Ok(result_rpc) = serde_json::from_str::<ServerId1>(&buf){
                        #[cfg(debug_assertions)]
                        debug!("收到开发者矿机返回 {:?}", result_rpc);
                        if result_rpc.id == CLIENT_LOGIN {
                        } else if result_rpc.id == CLIENT_SUBHASHRATE {
                        } else if result_rpc.id == CLIENT_GETWORK {
                        } else if result_rpc.result {
                        } else if result_rpc.id == 999{
                        } else {
                            crate::protocol::rpc::eth::handle_error_for_worker(&worker_name, &buf.as_bytes().to_vec());
                        }
                    } else if let Ok(job_rpc) =  serde_json::from_str::<ServerJobsWithHeight>(&buf) {
                        #[cfg(debug_assertions)]
                        debug!("收到开发者矿机任务 {:?}", job_rpc);
                        //send_job_to_client(state, job_rpc, &mut send_mine_jobs,&mut pool_w,&worker_name).await;
                        let diff = job_rpc.get_diff();

                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);

                        if diff == job_diff {
                            if let Some(job_id) = job_rpc.get_job_id() {
                                unsend_develop_jobs.push_back((job_id,job_rpc.result));
                            }
                        }

                    } else if let Ok(job_rpc) =  serde_json::from_str::<ServerSideJob>(&buf) {
                        #[cfg(debug_assertions)]
                        debug!("收到开发者矿机任务 {:?}", job_rpc);
                        let diff = job_rpc.get_diff();
                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);

                        if diff == job_diff {
                            if let Some(job_id) = job_rpc.get_job_id() {
                                unsend_develop_jobs.push_back((job_id,job_rpc.result));
                            }
                        }
                    } else if let Ok(job_rpc) =  serde_json::from_str::<Server>(&buf) {
                        #[cfg(debug_assertions)]
                        debug!("收到开发者矿机任务 {:?}", job_rpc);
                        let diff = job_rpc.get_diff();

                        job_diff_change(&mut job_diff,&job_rpc,&mut unsend_mine_jobs,&mut unsend_develop_jobs,&mut unsend_agent_jobs);
                        if diff == job_diff {
                            if let Some(job_id) = job_rpc.get_job_id() {
                                unsend_develop_jobs.push_back((job_id,job_rpc.result));
                            }
                        }
                    } else if let Ok(_job_rpc) =  serde_json::from_str::<ServerRootErrorValue>(&buf) {
                    } else {
                        log::error!("未找到的交易 {}",buf);
                        //write_to_socket_string(&mut pool_w, &buf, &worker_name).await;
                    }
                }
            },
            () = &mut sleep  => {
                // 发送本地旷工状态到远端。
                //info!("发送本地旷工状态到远端。{:?}",worker);
                match workers_queue.try_send(worker.clone()){
                    Ok(_) => {},
                    Err(_) => {log::warn!("发送旷工状态失败");},
                }

                sleep.as_mut().reset(time::Instant::now() + time::Duration::from_secs(60));
            },
        }
    }
}
