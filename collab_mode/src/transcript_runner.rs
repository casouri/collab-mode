// Public interface for listing, displaying, and running transcript tests
use std::path::{Path, PathBuf};
use std::collections::HashMap;

pub fn get_transcripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/server/transcripts")
}

pub fn list_transcripts() -> anyhow::Result<Vec<String>> {
    let dir = get_transcripts_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    
    let mut transcripts = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("txt") {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                transcripts.push(name.to_string());
            }
        }
    }
    
    transcripts.sort();
    Ok(transcripts)
}

pub fn display_transcript(transcript_name: &str) -> anyhow::Result<()> {
    // Add .txt if not present
    let filename = if transcript_name.ends_with(".txt") {
        transcript_name.to_string()
    } else {
        format!("{}.txt", transcript_name)
    };
    
    let transcript_path = get_transcripts_dir().join(&filename);
    if !transcript_path.exists() {
        return Err(anyhow::anyhow!("Transcript file not found: {}", filename));
    }
    
    let content = std::fs::read_to_string(&transcript_path)?;
    
    // Parse headers
    let mut lines = content.lines();
    let mut headers = HashMap::new();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    
    let test_name = headers.get("Name").cloned().unwrap_or_else(|| "Unnamed Test".to_string());
    
    println!("\nTranscript: {}", test_name);
    println!("File: {}", filename);
    println!("\n{}", "-".repeat(60));
    println!("{}", content);
    println!("{}", "-".repeat(60));
    
    Ok(())
}

#[cfg(feature = "test-runner")]
pub async fn run_single_transcript(transcript_name: &str) -> anyhow::Result<()> {
    use crate::server::transcript_tests::run_transcript_test;
    
    // Add .txt if not present
    let filename = if transcript_name.ends_with(".txt") {
        transcript_name.to_string()
    } else {
        format!("{}.txt", transcript_name)
    };
    
    let transcript_path = get_transcripts_dir().join(&filename);
    if !transcript_path.exists() {
        return Err(anyhow::anyhow!("Transcript file not found: {}", filename));
    }
    
    println!("Running transcript test: {}", filename);
    run_transcript_test(transcript_path.to_str().unwrap()).await?;
    println!("Test passed: {}", filename);
    
    Ok(())
}

#[cfg(not(feature = "test-runner"))]
pub async fn run_single_transcript(_transcript_name: &str) -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "Test runner not available. Build with --features test-runner to enable test execution."
    ))
}