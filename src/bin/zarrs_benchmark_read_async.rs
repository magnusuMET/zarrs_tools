use std::{sync::Arc, time::SystemTime};

use clap::Parser;
use futures::{FutureExt, StreamExt};
use zarrs::{
    array::{
        codec::{ArrayCodecTraits, CodecOptionsBuilder},
        concurrency::RecommendedConcurrency,
    },
    array_subset::ArraySubset,
    config::global_config,
    storage::{store::AsyncObjectStore, AsyncReadableStorageTraits},
};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = "Benchmark zarrs read throughput with the async API."
)]
struct Args {
    /// The zarr array directory.
    path: String,

    /// Number of concurrent chunks.
    #[arg(long)]
    concurrent_chunks: Option<usize>,

    /// Read the entire array in one operation.
    ///
    /// If set, `concurrent_chunks` is ignored.
    #[arg(long, default_value_t = false)]
    read_all: bool,

    /// Ignore checksums.
    ///
    /// If set, checksum validation in codecs (e.g. crc32c) is skipped.
    #[arg(long, default_value_t = false)]
    ignore_checksums: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    zarrs::config::global_config_mut().set_validate_checksums(!args.ignore_checksums);

    // let storage = Arc::new(AsyncOpendalStore::new({
    //     let mut builder = opendal::services::Fs::default();
    //     builder.root(&args.path.clone()); // FIXME: Absolute
    //     Operator::new(builder)?.finish()
    // }));
    let storage = Arc::new(AsyncObjectStore::new(
        object_store::local::LocalFileSystem::new_with_prefix(args.path.clone())?,
    ));
    let array = Arc::new(zarrs::array::Array::async_new(storage.clone(), "/").await?);
    // println!("{:#?}", array.metadata());

    let chunks = ArraySubset::new_with_shape(array.chunk_grid_shape().unwrap());
    let chunks_shape = chunks.shape();

    let start = SystemTime::now();
    let mut bytes_decoded = 0;
    let chunk_indices = (0..chunks.shape().iter().product())
        .map(|chunk_index| zarrs::array::unravel_index(chunk_index, chunks_shape))
        .collect::<Vec<_>>();
    if args.read_all {
        let subset = ArraySubset::new_with_shape(array.shape().to_vec());
        bytes_decoded += array.async_retrieve_array_subset(&subset).await?.len();
    } else {
        let chunk_representation =
            array.chunk_array_representation(&vec![0; array.chunk_grid().dimensionality()])?;
        let concurrent_target = std::thread::available_parallelism().unwrap().get();
        let (chunk_concurrent_limit, codec_concurrent_target) =
            zarrs::array::concurrency::calc_concurrency_outer_inner(
                concurrent_target,
                &if let Some(concurrent_chunks) = args.concurrent_chunks {
                    let concurrent_chunks =
                        std::cmp::min(chunks.num_elements_usize(), concurrent_chunks);
                    RecommendedConcurrency::new(concurrent_chunks..concurrent_chunks)
                } else {
                    let concurrent_chunks = std::cmp::min(
                        chunks.num_elements_usize(),
                        global_config().chunk_concurrent_minimum(),
                    );
                    RecommendedConcurrency::new_minimum(concurrent_chunks)
                },
                &array
                    .codecs()
                    .recommended_concurrency(&chunk_representation)?,
            );
        let codec_options = CodecOptionsBuilder::new()
            .concurrent_target(codec_concurrent_target)
            .build();

        let futures = chunk_indices
            .into_iter()
            .map(|chunk_indices| {
                // println!("Chunk/shard: {:?}", chunk_indices);
                let array = array.clone();
                let codec_options = codec_options.clone();
                async move {
                    array
                        .async_retrieve_chunk_opt(&chunk_indices, &codec_options)
                        .map(|bytes| bytes.map(|bytes| bytes.len()))
                        .await
                }
            })
            .map(tokio::task::spawn);
        let mut stream = futures::stream::iter(futures).buffer_unordered(chunk_concurrent_limit);
        while let Some(item) = stream.next().await {
            bytes_decoded += item.unwrap()?;
        }
    }
    let duration = SystemTime::now().duration_since(start)?.as_secs_f32();
    println!(
        "Decoded {} ({:.2}MB) in {:.2}ms ({:.2}MB decoded @ {:.2}GB/s)",
        args.path,
        storage.size().await? as f32 / 1e6,
        duration * 1e3,
        bytes_decoded as f32 / 1e6,
        (/* GB */bytes_decoded as f32 * 1e-9) / duration,
    );
    Ok(())
}
