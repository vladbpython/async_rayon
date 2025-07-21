use async_rayon::ThreadPoolInner;
use tokio::runtime::Builder;
use std::time::Instant;


fn main(){
    let rt = Builder::new_multi_thread()
    .worker_threads(64)
    .enable_all()
    .build()
    .unwrap();

    rt.block_on(async{
        let now = Instant::now();
        let pool = ThreadPoolInner::new_cpu();
        let _ = pool.scope_async(|scope|async move{
            for i in 0..5_000_000 {
                scope.spawn(async move {
                    let _a = i;
                }); 
            }
            
        }).await;
        pool.shutdown().await;
        println!("elapsed: {:?}",now.elapsed());
    });


}